use anyhow::Result;
use crossterm::cursor;
use crossterm::execute;
use crossterm::style::Stylize;

use crate::theme::{AuraStyle, Themed};
use rustyline::error::ReadlineError;
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;

use crate::api::stream::{StreamHandler, StreamResult};
use crate::api::types::{DisplayEvent, ShellCallDetail, ToolCallInfo, snake_to_pascal_case};
use crate::backend::Backend;
use crate::config::AppConfig;
use crate::event_names;
use crate::repl::commands;
use crate::repl::conversations::ConversationStore;
use crate::repl::history::ConversationHistory;
use crate::repl::input_reader::{self, HISTORY_COUNT, HISTORY_DEPTH, LAST_READLINE_INPUT};
use crate::tools;
use crate::ui::markdown::render_markdown;
use crate::ui::prompt::{
    PendingCommand, WaveAnimation, cleanup_terminal, clear_display_events, clear_input_hint,
    drain_stdin, erase_input_frame, extend_display_events, frame_lines, get_cumulative_tokens,
    get_selected_model, handle_ctrlc, install_sigint_handler, is_expanded_output,
    last_mid_stream_history_entry, load_and_restore_sse_events, lock_term,
    overwrite_orch_task_header_unlocked, prepare_input_line, print_fields_tree,
    print_tool_call_expanded, print_user_echo, print_welcome_state_animated, push_display_event,
    push_mid_stream_history, push_sse_event, random_bullet_color, redraw_input_frame,
    replay_event_log_global, reset_ctrlc_state, reset_input_geometry, restore_terminal_mode,
    seed_model_cache, seed_status_bar_tokens, set_expanded_output, set_mid_stream_history,
    set_noncanonical_noecho, set_processing, set_selected_model, set_status_bar_tokens,
    set_stream_conv_dir, set_welcome_state, setup_terminal, stop_and_clear_animation,
    styled_prompt, take_pending_command, take_queued_input, task_color_for, text_lines,
    update_status_bar, update_status_bar_unlocked, with_event_log, with_event_log_mut,
};
use crate::ui::welcome::WelcomeState;

/// Orchestrator task tracking — promoted to module scope so reasoning
/// helpers can look up which task a worker's reasoning belongs to.
pub(crate) struct OrchTaskInfo {
    pub header_line_num: u32,
    pub worker_id: String,
    pub tools: Vec<String>,
}

pub(crate) struct OrchDisplayState {
    pub tasks: std::collections::HashMap<String, OrchTaskInfo>,
}

/// Live reasoning state.
///
/// Top-level reasoning (no `parent_agent_id`) renders as a top-level
/// `● Reasoning` header followed by one or more `│ <body>` rows in scrollback.
/// Each `\n` in incoming content pushes a new body row; sub-line content
/// updates the current body row in place via cursor save/move/clear/print/restore.
///
/// Worker reasoning (has `parent_agent_id`) renders as a tree entry inside
/// the worker's task: `└─ ● Reasoning` plus a single `   ⎿ <body>` line that
/// updates in place as chunks arrive. Newlines in worker reasoning are
/// flattened to spaces so the body stays on one line — matching the format
/// of tool-call entries already in the tree.
///
/// Live updates use cursor SavePosition / MoveUp / Clear / Print /
/// RestorePosition (the same pattern the WaveAnimation thread uses for tool
/// duration in-place updates), so the cursor never lives mid-line and the
/// next event lands cleanly.
struct LiveReasoning {
    agent_id: String,
    /// Carried so we can persist it back into the DisplayEvent's fields map
    /// on flush; `is_worker` drives the rendering branch.
    #[allow(dead_code)]
    parent_agent_id: Option<String>,
    /// Task id for worker reasoning (filled in at first chunk via
    /// `orch_state` lookup). `None` for top-level reasoning.
    task_id: Option<String>,
    combined: String,
    fields: BTreeMap<String, serde_json::Value>,
    /// `true` once the header line has been printed to scrollback.
    #[allow(dead_code)]
    started: bool,
    /// Scrollback line number of the FIRST body row (top-level reasoning
    /// may now span multiple rows, growing as newlines arrive). For worker
    /// reasoning this is the single in-place updated row.
    body_line_num: u32,
    /// Text currently displayed on the body (worker: one row; top-level:
    /// multi-line content rendered across `body_visual_rows` rows).
    body_line_text: String,
    /// Number of scrollback rows the body region currently occupies. Always
    /// `1` for worker reasoning. For top-level reasoning this grows by one
    /// for each newline encountered in incoming chunks — the growth is
    /// committed to scrollback via a brief halt+extend+restart of the
    /// WaveAnimation (same pattern `tool_call_started` uses to push the
    /// frame down when a new tree entry is added).
    body_visual_rows: u32,
    is_worker: bool,
}

/// Worker reasoning body connector (`   ⎿ <text>`). Caller must hold
/// `lock_term()`.
fn write_worker_reasoning_body_line(text: &str) {
    print!(
        "{}{} {}",
        crate::ui::orchestrator::TREE_END_DURATION.themed(AuraStyle::Connector),
        "⎿".themed(AuraStyle::Connector),
        text.themed(AuraStyle::Muted),
    );
}

/// Flatten + truncate worker reasoning body to one terminal row.
fn worker_reasoning_body_for_terminal(text: &str) -> String {
    // Connector width: "   ⎿ " = 5 cols visible.
    let prefix_cols: usize = 5;
    let (term_w, _) = crate::ui::prompt::term_size();
    let avail = (term_w as usize).saturating_sub(prefix_cols).max(8);
    let flattened: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= avail {
        flattened
    } else {
        let truncated: String = flattened.chars().take(avail.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

/// In-place rewrite of the worker reasoning body row.
/// Caller must already hold `lock_term()`. The WaveAnimation is restarted
/// after the worker block opens, so the cursor lives at the input line and
/// we use the same `+3` offset that the animation thread uses for tool
/// durations (line 372 of `ui/animation.rs`).
fn rewrite_worker_reasoning_body_inplace(state: &LiveReasoning, body_text: &str) {
    use std::io::Write;
    let total_sb = crate::ui::prompt::current_orch_scrollback();
    let body_up = (total_sb + 3).saturating_sub(state.body_line_num);
    let (_, th) = crate::ui::prompt::term_size();
    if body_up == 0 || body_up >= th as u32 {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, crossterm::cursor::SavePosition);
    let _ = execute!(
        stdout,
        crossterm::cursor::MoveUp(body_up as u16),
        crossterm::cursor::MoveToColumn(0)
    );
    let _ = execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine)
    );
    write_worker_reasoning_body_line(body_text);
    let _ = execute!(stdout, crossterm::cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Render the top-level `● Reasoning` header and an opening `⎿ ` body row
/// in scrollback. Subsequent reasoning chunks update the body row in-place
/// via the same SavePosition/MoveUp/Clear/Print/RestorePosition pattern
/// that the animation thread uses for tool durations. The cursor lives at
/// the scrollback bottom (spinner is halted before this is called), so
/// the rewrite uses the bare `total_sb - body_line_num` distance.
///
/// `agent_id` is appended when it identifies the coordinator or some other
/// non-default agent (e.g., `● Reasoning - coordinator`); single-agent
/// deployments with `agent_id == "main"` render just `● Reasoning`.
///
/// Caller must already hold `lock_term()`.
fn open_top_level_reasoning_block(state: &mut LiveReasoning, agent_id: &str) {
    let bullet_color = task_color_for(if agent_id.is_empty() {
        "__orchestrator__"
    } else {
        agent_id
    });
    let header = if agent_id == "main" || agent_id.is_empty() {
        "Reasoning".to_string()
    } else {
        format!("Reasoning - {agent_id}")
    };
    println!(
        "{} {}",
        "●"
            .with(bullet_color)
            .attribute(crossterm::style::Attribute::Bold),
        header
            .as_str()
            .themed(AuraStyle::Primary)
            .attribute(crossterm::style::Attribute::Bold),
    );
    crate::ui::prompt::increment_orch_scrollback();
    // Body row: print "⎿ " (no content yet) and commit with a newline so
    // the body has a stable scrollback index. The `⎿` corner is final —
    // streaming chunks update only the body text (single-line, flattened);
    // multi-line content remains visible via `/expand` replay.
    print!("{} ", "⎿".themed(AuraStyle::Connector));
    println!();
    crate::ui::prompt::increment_orch_scrollback();
    state.body_line_num = crate::ui::prompt::current_orch_scrollback() - 1;
    state.body_line_text = String::new();
    state.started = true;
    // Trailing blank separator row — counted in scrollback so the next
    // event (Plan/Task/Thinking) has a visible gap from the body row.
    // The animation overlay is printed *below* this blank, so the
    // separator stays visible during streaming AND after teardown.
    println!();
    crate::ui::prompt::increment_orch_scrollback();
}

/// Render the worker tree entry: `└─ ● Reasoning` + initial empty body row.
/// Also upgrades any prior tool-call entry's connector from `└─` to `├─`
/// (via `register_orch_reasoning_in_tree`) and registers this entry in
/// `ORCH_LAST_TOOL_LINES` so the next tool call can upgrade reasoning's
/// connector in turn. Caller must already hold `lock_term()`.
fn open_worker_reasoning_block(state: &mut LiveReasoning, task_id: &str) {
    crate::ui::orchestrator::register_orch_reasoning_in_tree(
        task_id,
        |bullet_line_num, body_line_num| {
            state.body_line_num = body_line_num;
            state.body_line_text = String::new();
            state.started = true;
            let _ = bullet_line_num;
        },
    );
}

/// Apply a chunk of content to the currently-open live block.
/// Caller must already hold `lock_term()`.
///
/// Both top-level and worker reasoning use in-place body rewrites so the
/// cursor never leaves the input line — the WaveAnimation thread can keep
/// ticking in parallel. Both use the animation's `+3` offset (same as the
/// in-flight tool duration updates in `ui/animation.rs`).
///
/// Newlines in incoming content are flattened to spaces — the live body
/// stays on one terminal row to avoid wrapping into the animation rows.
/// The full multi-line content is preserved in `DisplayEvent::Reasoning`
/// for `/expand` replay.
fn apply_reasoning_chunk(state: &mut LiveReasoning, delta: &str) {
    state.combined.push_str(delta);
    state.body_line_text.push_str(delta);
    if state.is_worker {
        let display = worker_reasoning_body_for_terminal(&state.body_line_text);
        rewrite_worker_reasoning_body_inplace(state, &display);
    } else {
        let lines = top_level_reasoning_body_for_terminal(&state.body_line_text);
        rewrite_top_level_reasoning_body_inplace(state, &lines);
    }
}

/// Split top-level body content into one display string per visual row.
/// Each logical line (separated by `\n` in the streamed content) is
/// flattened (consecutive whitespace collapsed) and then word-wrapped to
/// fit `term_width - 2` columns (the 2 columns are reserved for the
/// `⎿ ` / `  ` row prefix). The wrap prefers word boundaries; if a single
/// token is longer than the available width it hard-wraps in the middle.
///
/// Returns at least one (possibly empty) string so the body always has a
/// visible row.
fn top_level_reasoning_body_for_terminal(text: &str) -> Vec<String> {
    let prefix_cols: usize = 2;
    let (term_w, _) = crate::ui::prompt::term_size();
    let avail = (term_w as usize).saturating_sub(prefix_cols).max(8);
    let mut visual: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let flat: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.is_empty() {
            visual.push(String::new());
            continue;
        }
        let chars: Vec<char> = flat.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let remaining = chars.len() - start;
            if remaining <= avail {
                let seg: String = chars[start..].iter().collect();
                visual.push(seg);
                break;
            }
            let hard_end = start + avail;
            // Prefer a soft break at the last space within [start, hard_end).
            let mut soft_end = hard_end;
            while soft_end > start && chars[soft_end - 1] != ' ' {
                soft_end -= 1;
            }
            let wrap_end = if soft_end > start { soft_end } else { hard_end };
            let seg: String = chars[start..wrap_end].iter().collect();
            // Strip the trailing space we broke at, if any.
            visual.push(seg.trim_end().to_string());
            start = wrap_end;
            // Skip the run of spaces we just broke on.
            while start < chars.len() && chars[start] == ' ' {
                start += 1;
            }
        }
    }
    if visual.is_empty() {
        visual.push(String::new());
    }
    visual
}

/// In-place rewrite of the top-level body. The WaveAnimation is running
/// (cursor at the input line); we use the same `+3` offset that the
/// animation thread uses for tool durations so `MoveUp` lands on the body's
/// FIRST scrollback row, above the anim overlay. Subsequent body rows are
/// rewritten by `MoveDown`-ing one row at a time.
///
/// `state.body_visual_rows` rows in scrollback are owned by the body — the
/// caller must extend that allocation BEFORE calling this with more logical
/// lines than the body currently has rows for (otherwise the trailing
/// logical lines are silently dropped).
fn rewrite_top_level_reasoning_body_inplace(state: &LiveReasoning, lines: &[String]) {
    use std::io::Write;
    let total_sb = crate::ui::prompt::current_orch_scrollback();
    let body_up = (total_sb + 3).saturating_sub(state.body_line_num);
    let (_, th) = crate::ui::prompt::term_size();
    if body_up == 0 || body_up >= th as u32 {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, crossterm::cursor::SavePosition);
    let _ = execute!(
        stdout,
        crossterm::cursor::MoveUp(body_up as u16),
        crossterm::cursor::MoveToColumn(0)
    );

    let rows = state.body_visual_rows as usize;
    for i in 0..rows {
        let _ = execute!(
            stdout,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine)
        );
        // First row gets the `⎿ ` connector (descent from the bullet);
        // subsequent rows use 2 spaces so the body content stays aligned
        // visually under the connector.
        let connector = if i == 0 { "⎿" } else { " " };
        let content = lines.get(i).map(|s| s.as_str()).unwrap_or("");
        print!(
            "{} {}",
            connector.themed(AuraStyle::Connector),
            content.themed(AuraStyle::Muted),
        );
        if i + 1 < rows {
            let _ = execute!(
                stdout,
                crossterm::cursor::MoveDown(1),
                crossterm::cursor::MoveToColumn(0)
            );
        }
    }
    let _ = execute!(stdout, crossterm::cursor::RestorePosition);
    let _ = stdout.flush();
}

/// Persist the live reasoning block as a `DisplayEvent::Reasoning` so
/// replay/resume see one block per agent stretch.
///
/// Returns `true` when a top-level block was flushed — the caller (the
/// orchestrator-event handler) uses this to defer a separator blank row
/// until AFTER `erase_input_frame` has run, so the println lands in
/// scrollback rather than clobbering the live input frame.
///
/// No terminal output happens inside this function: the body row already
/// lives in scrollback at `body_line_num`, and the deferred separator
/// (when applicable) is committed by the caller.
fn flush_live_reasoning(state: &Arc<Mutex<Option<LiveReasoning>>>) -> bool {
    let Some(s) = state.lock().ok().and_then(|mut g| g.take()) else {
        return false;
    };
    if s.combined.is_empty() {
        return false;
    }
    let was_top_level = !s.is_worker;
    let mut fields = s.fields;
    fields.insert(
        "content".to_string(),
        serde_json::Value::String(s.combined.clone()),
    );
    push_display_event(DisplayEvent::Reasoning {
        content: s.combined,
        agent_id: s.agent_id,
        fields,
    });
    was_top_level
}

/// Orchestrator events that actually `println!` scrollback content.
/// `flush_live_reasoning` must run before these so the open reasoning block
/// closes cleanly and the next event lands on its own row. Events that only
/// touch the spinner sub-line or do in-place updates (`worker_reasoning`,
/// `synthesizing`) are deliberately excluded — flushing on them would slice
/// multi-chunk reasoning into separate `DisplayEvent::Reasoning` entries, one
/// per delta.
fn orch_event_prints_scrollback(event_name: &str) -> bool {
    matches!(
        event_name,
        event_names::PLAN_CREATED
            | event_names::TASK_STARTED
            | event_names::TOOL_CALL_STARTED
            | event_names::TOOL_CALL_COMPLETED
            | event_names::TASK_COMPLETED
            | event_names::ITERATION_COMPLETE
    )
}

pub fn run_repl(
    rt: &Runtime,
    config: AppConfig,
    mut permissions: crate::permissions::PermissionChecker,
    backend: &Backend,
    post_launch_warning: Option<String>,
) -> Result<()> {
    let mut conversation = ConversationHistory::new(config.system_prompt.as_deref());

    // Catch SIGINT so Ctrl-C works even when ISIG is unexpectedly enabled.
    install_sigint_handler();

    // Reset shared globals for this REPL session
    clear_display_events();
    set_expanded_output(false);
    // Initialize selected model from config (so config/CLI --model is respected)
    set_selected_model(config.model.clone());
    backend.setup_model_cache(&config);
    let mut conv_store: Option<ConversationStore> = ConversationStore::new().ok();
    // Per-process UUID used as the chat-session-id fallback when ConversationStore
    // can't be created (e.g. ~/.aura/conversations is unwritable). Without it, every
    // request would share a blank x-chat-session-id and collapse server-side
    // tracing/cancellation across unrelated turns.
    let fallback_session_uuid = uuid::Uuid::new_v4().to_string();
    set_stream_conv_dir(conv_store.as_ref().map(|s| s.dir().to_path_buf()));

    // Persist the resolved system prompt for this conversation (enables comparison on resume)
    if let (Some(store), Some(prompt)) = (&conv_store, &config.system_prompt) {
        store.save_system_prompt(prompt);
    }

    // Context compaction state
    let mut last_compact_prompt_threshold: u64 = 2_000_000;
    let mut compact_hint_pending = false;

    // Handle --resume flag: load conversation from disk
    if let Some(ref resume_id) = config.resume {
        match commands::resume_conversation(resume_id, config.system_prompt.as_deref()) {
            Some((store, history, events, was_expanded, _usage_totals)) => {
                // Delete the empty conversation that was just created
                if let Some(old) = conv_store.take() {
                    old.delete();
                }
                conv_store = Some(store);
                set_stream_conv_dir(conv_store.as_ref().map(|s| s.dir().to_path_buf()));
                conversation = history;
                clear_display_events();
                extend_display_events(events);
                set_expanded_output(was_expanded);
                // Restore SSE stream panel events
                if let Some(ref s) = conv_store {
                    load_and_restore_sse_events(s.dir());

                    if let Some(model) = s.load_model() {
                        set_selected_model(Some(model));
                    }

                    // Restore model cache from resumed conversation
                    if let Some(models) = s.load_models_cache() {
                        seed_model_cache(models);
                    }

                    // If system prompt was changed during resume conflict resolution, persist it
                    if let Some(ref prompt) = config.system_prompt {
                        s.save_system_prompt(prompt);
                    }
                }
            }
            None => {
                // resume_conversation already printed the error
                anyhow::bail!("Could not resume conversation '{}'", resume_id);
            }
        }
    }

    let history_path = input_reader::get_history_path();
    let mut input_reader: rustyline::Editor<
        input_reader::AuraHelper,
        rustyline::history::FileHistory,
    > = input_reader::create_input_reader()?;
    // Load per-conversation input history when resuming; otherwise start fresh.
    HISTORY_DEPTH.store(0, Ordering::Relaxed);
    if let Some(ref store) = conv_store {
        let entries = store.load_input_history();
        HISTORY_COUNT.store(entries.len(), Ordering::Relaxed);
        for entry in &entries {
            let _ = input_reader.add_history_entry(entry);
        }
        set_mid_stream_history(entries);
    } else {
        HISTORY_COUNT.store(0, Ordering::Relaxed);
        set_mid_stream_history(Vec::new());
    }

    // Pick the welcome content + colors once; reused on /expand and /resume replays.
    set_welcome_state(WelcomeState::pick());
    // Visual flourish gate: only run the fade-in animation under `--pretty`.
    // Default is the static print so logs/CI/screen-readers stay predictable.
    if config.pretty {
        print_welcome_state_animated();
    } else {
        crate::ui::prompt::print_welcome_state();
    }

    if config
        .model
        .as_deref()
        .is_some_and(|m| m.eq_ignore_ascii_case("aura-bootstrap"))
    {
        use crate::theme::Themed;
        println!(
            "\n  {} You're connected to the bootstrap configuration agent.\n  \
             Describe what you want this AURA instance to do and it will\n  \
             build a configuration for you.\n",
            "aura-bootstrap".themed_with(crate::theme::theme().heading),
        );
    }

    setup_terminal();

    // If resuming, replay the event log so the user sees the conversation
    let has_events = with_event_log(|log| !log.is_empty());
    if config.resume.is_some() && has_events {
        erase_input_frame();
        replay_event_log_global();
        // Seed token counters from the authoritative usage JSONL after replay
        // (replay resets + re-accumulates from view events; this ensures the
        // JSONL totals are the final source of truth).
        if let Some(ref store) = conv_store {
            let (p, c) = store.load_usage_totals();
            seed_status_bar_tokens(p, c);
        }
        println!(
            "{}",
            "Resumed conversation. Continue below.".themed(AuraStyle::Success),
        );
        println!();
        redraw_input_frame();
    }

    // Show post-launch warning if any (A2, C1, C2, C3 scenarios)
    if let Some(ref warning) = post_launch_warning {
        erase_input_frame();
        println!(
            "{}",
            format!("Warning: {}", warning).themed(AuraStyle::Warning),
        );
        println!();
        redraw_input_frame();
    }

    let input_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    // Restore any partially-typed input from a previous session
    let mut initial_input = conv_store
        .as_ref()
        .and_then(|s| s.load_pending_input())
        .unwrap_or_default();
    let mut auto_submit = false;

    loop {
        // Frame is already drawn (by setup or previous iteration).
        // Restore normal terminal mode and show cursor for readline.
        let _ = execute!(io::stdout(), cursor::Show);
        restore_terminal_mode();

        let was_auto_submit = auto_submit;
        let readline = if auto_submit {
            // Queued input — skip readline entirely
            let queued = std::mem::take(&mut initial_input);
            auto_submit = false;
            Ok(queued)
        } else if initial_input.is_empty() {
            input_reader.readline(&styled_prompt())
        } else {
            let result = input_reader.readline_with_initial(&styled_prompt(), (&initial_input, ""));
            initial_input.clear();
            result
        };

        match readline {
            Ok(line) => {
                clear_input_hint();
                reset_ctrlc_state();
                // Clear the snapshot — this input was submitted, not abandoned.
                if let Ok(mut g) = LAST_READLINE_INPUT.lock() {
                    g.clear();
                }
                let input = line.trim().to_string();
                if input.is_empty() {
                    if !was_auto_submit {
                        // Readline's Enter moved cursor to bottom border line.
                        // Move back up to input line so the frame is consistent.
                        let _ = execute!(io::stdout(), cursor::MoveUp(1));
                        // Refresh status bar in case a hint (e.g. "?") was showing
                        update_status_bar();
                    }
                    continue;
                }

                // Skip contiguous duplicate history entries so up/down
                // navigation jumps past repeated identical inputs.
                let is_dup = last_mid_stream_history_entry().as_deref() == Some(input.as_str());
                if !is_dup {
                    let _ = input_reader.add_history_entry(&input);
                    HISTORY_COUNT.fetch_add(1, Ordering::Relaxed);
                }
                HISTORY_DEPTH.store(0, Ordering::Relaxed);
                push_mid_stream_history(input.clone());

                if was_auto_submit {
                    // Auto-submit: frame is already drawn, just erase and echo
                    let _ = execute!(io::stdout(), cursor::Hide);
                    erase_input_frame();
                    reset_input_geometry();
                    print_user_echo(&input);
                    println!();
                } else {
                    // After Enter, rustyline moved cursor one row below the text end
                    // (row = text_lines). Navigate to the frame's last input row
                    // (row = frame_lines - 1) so erase_input_frame works correctly.
                    // With a deferred shrink the frame may be further down than
                    // the text, so we can't just MoveUp(1).
                    let _ = execute!(io::stdout(), cursor::Hide);
                    let fl = frame_lines() as i32;
                    let tl = text_lines() as i32;
                    let move_to_frame_end = (fl - 1) - tl; // normally -1
                    if move_to_frame_end >= 0 {
                        let _ = execute!(io::stdout(), cursor::MoveDown(move_to_frame_end as u16));
                    } else {
                        let _ = execute!(io::stdout(), cursor::MoveUp((-move_to_frame_end) as u16));
                    }
                    erase_input_frame();
                    reset_input_geometry();
                }

                if input == "/quit" || input == "/exit" {
                    // Save before exiting, or delete if conversation was never started
                    if let Some(ref store) = conv_store {
                        if conversation.messages().len() > 1 {
                            with_event_log(|log| {
                                store.save_all(conversation.messages(), log, is_expanded_output())
                            });
                        } else {
                            store.delete();
                        }
                    }
                    break;
                } else if input == "/clear" {
                    commands::handle_clear(&mut conversation, &mut conv_store, &mut input_reader);
                    continue;
                } else if input == "/help" {
                    commands::handle_help();
                    continue;
                } else if input == "/expand" {
                    commands::handle_expand(&conversation, &conv_store);
                    continue;
                } else if input == "/stream" {
                    commands::handle_stream();
                    continue;
                } else if input == "/conversations" {
                    commands::handle_conversations();
                    continue;
                } else if let Some(arg) = input.strip_prefix("/rename") {
                    commands::handle_rename(arg.trim(), &conv_store);
                    continue;
                } else if let Some(arg) = input.strip_prefix("/resume") {
                    // In-REPL resume ignores CLI --model and --system-prompt;
                    // uses whatever the resumed conversation had saved.
                    if let Some(new_input) = commands::handle_resume(
                        arg.trim(),
                        &mut conversation,
                        &mut conv_store,
                        &mut input_reader,
                        None,
                    ) {
                        initial_input = new_input;
                    }
                    continue;
                } else if let Some(filter) = input.strip_prefix("/model") {
                    let filter = filter.trim();
                    commands::handle_model(filter, &conv_store);
                    continue;
                } else if let Some(arg) = input.strip_prefix("/style") {
                    commands::handle_style(arg.trim());
                    continue;
                } else if input.starts_with('/') {
                    // Unknown command — don't send to API
                    println!("Unknown command: {}", input);
                    println!("Type /help for available commands.");
                    redraw_input_frame();
                    continue;
                }

                // Append compaction hint to user message if pending
                if compact_hint_pending {
                    compact_hint_pending = false;
                    let tokens = get_cumulative_tokens();
                    let augmented = format!(
                        "{}\n\n[System note: Context is at {} tokens. \
                         Ask the user if they'd like to compact the conversation \
                         using the CompactContext tool to free context space.]",
                        input, tokens
                    );
                    conversation.add_user(&augmented);
                } else {
                    conversation.add_user(&input);
                }

                // Persist: set conversation name from first user input
                if let Some(ref store) = conv_store {
                    store.set_name_if_empty(&input);
                }

                // Track event_log length so we can find new usage events after the turn
                let event_log_turn_start = with_event_log(|log| log.len());

                // Echo user input then start animation (which draws the frame)
                print_user_echo(&input);
                println!();

                // Record UserInput event now (before streaming) so it
                // precedes any orchestrator events pushed during the turn.
                push_display_event(DisplayEvent::UserInput(input.clone()));

                // Clear input buffer and enter non-canonical no-echo mode for processing
                if let Ok(mut buf) = input_buf.lock() {
                    buf.clear();
                }
                set_noncanonical_noecho();
                set_processing(true);
                crate::ui::prompt::clear_agent_reasoning();
                crate::ui::prompt::reset_orch_tools();

                // Per-turn event recording state
                let pending_args: Arc<
                    Mutex<std::collections::HashMap<String, BTreeMap<String, serde_json::Value>>>,
                > = Arc::new(Mutex::new(std::collections::HashMap::new()));
                let turn_events: Arc<Mutex<Vec<DisplayEvent>>> = Arc::new(Mutex::new(Vec::new()));
                let cancel_flag = Arc::new(AtomicBool::new(false));

                // Live reasoning block — accumulated as chunks stream in,
                // closed by `flush_live_reasoning` on the next non-reasoning
                // event (or end of stream). Reasoning is NOT routed through
                // `set_agent_reasoning` — that sub-line is reserved for the
                // `_aura_reasoning` blurbs attached to tool calls and to
                // `aura.progress` messages.
                let live_reasoning: Arc<Mutex<Option<LiveReasoning>>> = Arc::new(Mutex::new(None));

                let (anim, stop_flag) = WaveAnimation::start(
                    "Thinking",
                    vec![],
                    input_buf.clone(),
                    Some(cancel_flag.clone()),
                );
                let mut anim = Some(anim);
                prepare_input_line(&input_buf, Some(&cancel_flag));

                let anim_cleared = Arc::new(AtomicBool::new(false));

                let cancel_for_stream = cancel_flag.clone();

                #[allow(clippy::type_complexity)]
                let post_tool_wave: Arc<
                    Mutex<Option<(WaveAnimation, Arc<AtomicBool>)>>,
                > = Arc::new(Mutex::new(None));

                // --- Orchestrator display state ---
                let orch_state: Arc<Mutex<OrchDisplayState>> =
                    Arc::new(Mutex::new(OrchDisplayState {
                        tasks: std::collections::HashMap::new(),
                    }));
                // Set after tool_call_completed; consumed before the next
                // non-tool event to insert a blank line separator.
                let needs_blank = Arc::new(AtomicBool::new(false));

                // Build tool defs only when client tools are enabled. When
                // disabled the CLI sends no `tools` field, the model has no
                // local tools to call, and the tool-execution branches in
                // this loop simply never fire.
                let tool_defs: Vec<_> = if config.enable_client_tools {
                    tools::client_tool_definitions()
                } else {
                    Vec::new()
                };
                let tool_defs_arg: Option<&[_]> = if config.enable_client_tools {
                    Some(&tool_defs)
                } else {
                    None
                };
                let mut tool_loop_error: Option<anyhow::Error> = None;
                let mut final_text = String::new();
                let mut did_compact = false;
                let mut auto_compact_count: u32 = 0;

                // --- Update grouping state (persists across tool_loop iterations) ---
                struct UpdateContext {
                    file_path: String,
                    snapshot: Option<String>,
                    shell_calls: Vec<ShellCallDetail>,
                    commands_used: Vec<String>,
                    start_time: std::time::Instant,
                }

                /// Finalize an active UpdateContext: compute diff, display,
                /// record DisplayEvent.
                fn finalize_update(
                    ctx: UpdateContext,
                    turn_events: &Arc<Mutex<Vec<DisplayEvent>>>,
                ) {
                    let new_content = std::fs::read_to_string(&ctx.file_path).unwrap_or_default();
                    let old_content = ctx.snapshot.as_deref().unwrap_or("");
                    let (diff_text, lines_added, lines_removed) =
                        tools::compute_diff(old_content, &new_content);
                    let duration = ctx.start_time.elapsed();

                    // Header: "Updated 1 file"
                    let header = tools::format_tool_group_header("Update", 1);
                    println!(
                        "{} {}",
                        "●"
                            .with(random_bullet_color())
                            .attribute(crossterm::style::Attribute::Bold),
                        header.themed(AuraStyle::Primary),
                    );

                    // File path
                    println!(
                        "{} {}",
                        "├─".themed(AuraStyle::Connector),
                        ctx.file_path.as_str().themed(AuraStyle::Muted),
                    );

                    if ctx.shell_calls.is_empty() {
                        println!(
                            "{} {}",
                            "└─".themed(AuraStyle::Connector),
                            "No changes made".themed(AuraStyle::Muted),
                        );
                    } else {
                        // Summary before diff
                        tools::print_update_summary(lines_added, lines_removed, "└─");
                        tools::print_update_diff(&diff_text, 10);
                    }
                    println!();

                    if let Ok(mut events) = turn_events.lock() {
                        events.push(DisplayEvent::FileUpdate {
                            file_path: ctx.file_path,
                            commands_used: ctx.commands_used,
                            shell_calls: ctx.shell_calls,
                            diff_text,
                            lines_added,
                            lines_removed,
                            duration,
                        });
                    }
                }

                let mut active_update: Option<UpdateContext> = None;
                // Shared flag so streaming callbacks can suppress Shell display
                // when the LLM is inside an Update group.
                let in_update_group = Arc::new(AtomicBool::new(false));

                // Tool execution loop: send request → process stream → if tool calls,
                // execute locally and send results back → repeat until text response.
                'tool_loop: loop {
                    let chat_session_id = crate::api::session::resolve_chat_session_id(
                        &config.extra_headers,
                        conv_store
                            .as_ref()
                            .map(|s| s.uuid.as_str())
                            .unwrap_or(&fallback_session_uuid),
                        crate::api::session::SessionKind::Chat,
                    );
                    let result = rt.block_on(async {
                        let mut handler = ReplStreamHandler {
                            pending_args: pending_args.clone(),
                            turn_events: turn_events.clone(),
                            live_reasoning: live_reasoning.clone(),
                            stop_flag: stop_flag.clone(),
                            anim_cleared: anim_cleared.clone(),
                            cancel: cancel_flag.clone(),
                            post_tool_wave: post_tool_wave.clone(),
                            input_buf: input_buf.clone(),
                            orch_state: orch_state.clone(),
                            needs_blank: needs_blank.clone(),
                            in_update_group: in_update_group.clone(),
                        };
                        backend
                            .stream_chat(
                                conversation.messages(),
                                tool_defs_arg,
                                &chat_session_id,
                                cancel_for_stream.clone(),
                                &mut handler,
                            )
                            .await
                    });

                    // Close any live reasoning block left open by the stream
                    // (e.g., reasoning was the last thing the model emitted
                    // before the stream ended without a usage/tool event).
                    flush_live_reasoning(&live_reasoning);

                    // Check for cancellation
                    if cancel_flag.load(Ordering::Relaxed) {
                        break 'tool_loop;
                    }

                    match result {
                        Ok(StreamResult::TextResponse(text)) => {
                            // Check for auto-compaction trigger
                            let tokens = get_cumulative_tokens();
                            if tokens >= 8_000_000
                                && text.contains(
                                    "My tools returned more data than I can work with at once",
                                )
                            {
                                auto_compact_count += 1;

                                if auto_compact_count <= 2 {
                                    // Auto-compact and retry
                                    let removed = conversation.compact();

                                    // Stop any running animations
                                    if let Ok(mut guard) = post_tool_wave.lock()
                                        && let Some((ptw_anim, _)) = guard.take()
                                    {
                                        ptw_anim.finish();
                                    }
                                    if !anim_cleared.load(Ordering::Relaxed) {
                                        if let Some(a) = anim.take() {
                                            a.finish();
                                        }
                                        anim_cleared.store(true, Ordering::Relaxed);
                                    }
                                    erase_input_frame();

                                    println!(
                                        "{} {}",
                                        "●"
                                            .with(random_bullet_color())
                                            .attribute(crossterm::style::Attribute::Bold),
                                        "Auto-compacting context"
                                            .attribute(crossterm::style::Attribute::Bold),
                                    );
                                    println!(
                                        "{} Context limit reached — removed {} messages ({} total tokens)",
                                        "└─".themed(AuraStyle::Connector),
                                        removed,
                                        tokens,
                                    );
                                    println!();

                                    if let Ok(mut events) = turn_events.lock() {
                                        events.push(DisplayEvent::Compacted {
                                            messages_removed: removed,
                                        });
                                    }
                                    did_compact = true;

                                    // Restart thinking animation and retry with compacted context
                                    let (new_anim, new_stop) = WaveAnimation::start(
                                        "Thinking",
                                        vec![],
                                        input_buf.clone(),
                                        Some(cancel_flag.clone()),
                                    );
                                    drop(new_stop);
                                    if let Ok(mut guard) = post_tool_wave.lock() {
                                        *guard = Some((new_anim, stop_flag.clone()));
                                    }
                                    prepare_input_line(&input_buf, Some(&cancel_flag));

                                    continue 'tool_loop;
                                } else {
                                    // 3rd attempt: history-free fallback
                                    // Compact client-side one more time
                                    let removed = conversation.compact();

                                    if let Ok(mut guard) = post_tool_wave.lock()
                                        && let Some((ptw_anim, _)) = guard.take()
                                    {
                                        ptw_anim.finish();
                                    }
                                    if !anim_cleared.load(Ordering::Relaxed) {
                                        if let Some(a) = anim.take() {
                                            a.finish();
                                        }
                                        anim_cleared.store(true, Ordering::Relaxed);
                                    }
                                    erase_input_frame();

                                    println!(
                                        "{} {}",
                                        "●"
                                            .with(random_bullet_color())
                                            .attribute(crossterm::style::Attribute::Bold),
                                        "Auto-compacting context (recovery mode)"
                                            .attribute(crossterm::style::Attribute::Bold),
                                    );
                                    println!(
                                        "{} Removed {} more messages. Sending minimal recovery request.",
                                        "└─".themed(AuraStyle::Connector),
                                        removed,
                                    );
                                    println!();

                                    if let Ok(mut events) = turn_events.lock() {
                                        events.push(DisplayEvent::Compacted {
                                            messages_removed: removed,
                                        });
                                    }
                                    did_compact = true;

                                    // Send a minimal history-free message so the LLM can recover
                                    conversation.add_assistant(&text);
                                    conversation.add_user(
                                        "The conversation context was too large so it has been \
                                         compacted automatically. Please continue where you left off."
                                    );

                                    // Restart animation and retry
                                    let (new_anim, new_stop) = WaveAnimation::start(
                                        "Thinking",
                                        vec![],
                                        input_buf.clone(),
                                        Some(cancel_flag.clone()),
                                    );
                                    drop(new_stop);
                                    if let Ok(mut guard) = post_tool_wave.lock() {
                                        *guard = Some((new_anim, stop_flag.clone()));
                                    }
                                    prepare_input_line(&input_buf, Some(&cancel_flag));

                                    continue 'tool_loop;
                                }
                            }

                            final_text = text;
                            break 'tool_loop;
                        }
                        Ok(StreamResult::ToolCalls {
                            text,
                            tool_calls,
                            server_results,
                        }) => {
                            // Stop any running animations before tool execution
                            if let Ok(mut guard) = post_tool_wave.lock()
                                && let Some((ptw_anim, _)) = guard.take()
                            {
                                ptw_anim.finish();
                            }
                            if !anim_cleared.load(Ordering::Relaxed) {
                                if let Some(a) = anim.take() {
                                    a.finish();
                                }
                                anim_cleared.store(true, Ordering::Relaxed);
                            }
                            erase_input_frame();

                            // Convert AccumulatedToolCalls to ToolCallInfo for history
                            let tool_call_infos: Vec<ToolCallInfo> = tool_calls
                                .iter()
                                .map(|tc| ToolCallInfo {
                                    id: tc.id.clone(),
                                    call_type: "function".to_string(),
                                    function: crate::api::types::FunctionCallInfo {
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.clone(),
                                    },
                                })
                                .collect();

                            // Add assistant message with tool calls to history
                            let text_content = if text.is_empty() { None } else { Some(text) };
                            conversation
                                .add_assistant_with_tool_calls(text_content, tool_call_infos);

                            // Execute each tool call, collecting info for grouped display
                            let mut batch_tools: Vec<(
                                String,
                                String,
                                String,
                                std::time::Duration,
                            )> = Vec::new();
                            for tc in &tool_calls {
                                // Special handling for CompactContext — needs direct
                                // access to conversation and event_log
                                if tc.name == "CompactContext" {
                                    let removed = conversation.compact();
                                    let result_msg = format!(
                                        "Context compacted: removed {} messages. \
                                         Conversation history has been pruned to the most \
                                         recent half. The system prompt is preserved.",
                                        removed
                                    );

                                    println!(
                                        "{} {}",
                                        "●"
                                            .with(random_bullet_color())
                                            .attribute(crossterm::style::Attribute::Bold),
                                        "CompactContext()".themed(AuraStyle::Primary),
                                    );
                                    println!(
                                        "{} {}",
                                        "└─".themed(AuraStyle::Connector),
                                        result_msg.as_str().themed(AuraStyle::Muted),
                                    );

                                    if let Ok(mut events) = turn_events.lock() {
                                        events.push(DisplayEvent::Compacted {
                                            messages_removed: removed,
                                        });
                                    }
                                    did_compact = true;

                                    conversation.add_tool_result(&tc.id, &tc.name, &result_msg);
                                    continue;
                                }

                                // --- Update tool grouping ---
                                if tc.name == "Update" {
                                    // Finalize any previous active Update
                                    if let Some(prev) = active_update.take() {
                                        finalize_update(prev, &turn_events);
                                    }

                                    let args: serde_json::Value =
                                        serde_json::from_str(&tc.arguments).unwrap_or_default();
                                    let file_path =
                                        args["file_path"].as_str().unwrap_or("?").to_string();

                                    // Show what we're about to do before asking permission
                                    let display =
                                        tools::format_tool_call_display(&tc.name, &tc.arguments);
                                    println!(
                                        "{} {}",
                                        "●"
                                            .with(random_bullet_color())
                                            .attribute(crossterm::style::Attribute::Bold),
                                        display.themed(AuraStyle::Primary),
                                    );

                                    // Check permissions for Update
                                    let perm = permissions.check(&tc.name, &tc.arguments);
                                    match perm {
                                        crate::permissions::PermissionResult::Denied(reason) => {
                                            in_update_group.store(false, Ordering::Relaxed);
                                            eprintln!(
                                                "  {}",
                                                reason.as_str().themed(AuraStyle::Warning)
                                            );
                                            let rules = permissions.describe_rules();
                                            let denied_msg = tools::permission_denied_message(
                                                &tc.name,
                                                &reason,
                                                rules.as_deref(),
                                            );
                                            conversation.add_tool_result(
                                                &tc.id,
                                                &tc.name,
                                                &denied_msg,
                                            );
                                            continue;
                                        }
                                        crate::permissions::PermissionResult::Prompt => {
                                            if !permissions
                                                .prompt_tool_permission(&tc.name, &tc.arguments)
                                            {
                                                in_update_group.store(false, Ordering::Relaxed);
                                                let reason = "denied by user".to_string();
                                                let rules = permissions.describe_rules();
                                                let denied_msg = tools::permission_denied_message(
                                                    &tc.name,
                                                    &reason,
                                                    rules.as_deref(),
                                                );
                                                conversation.add_tool_result(
                                                    &tc.id,
                                                    &tc.name,
                                                    &denied_msg,
                                                );
                                                continue;
                                            }
                                        }
                                        crate::permissions::PermissionResult::Allowed => {}
                                    }

                                    // Snapshot the file
                                    let snapshot = std::fs::read_to_string(&file_path).ok();

                                    active_update = Some(UpdateContext {
                                        file_path,
                                        snapshot,
                                        shell_calls: Vec::new(),
                                        commands_used: Vec::new(),
                                        start_time: std::time::Instant::now(),
                                    });

                                    let result_msg = format!(
                                        "Update context started for {}. Use Shell calls to make changes.",
                                        active_update.as_ref().unwrap().file_path
                                    );
                                    conversation.add_tool_result(&tc.id, &tc.name, &result_msg);
                                    continue;
                                }

                                // --- Shell within an active Update group ---
                                if tc.name == "Shell" && active_update.is_some() {
                                    // Run the full permission check — Update
                                    // approval does not implicitly trust
                                    // arbitrary Shell commands. The user must
                                    // allow each command (or pattern)
                                    // explicitly.
                                    let shell_perm = permissions.check("Shell", &tc.arguments);
                                    match shell_perm {
                                        crate::permissions::PermissionResult::Denied(reason) => {
                                            eprintln!(
                                                "  {}",
                                                reason.as_str().themed(AuraStyle::Warning)
                                            );
                                            let rules = permissions.describe_rules();
                                            let denied_msg = tools::permission_denied_message(
                                                "Shell",
                                                &reason,
                                                rules.as_deref(),
                                            );
                                            conversation.add_tool_result(
                                                &tc.id,
                                                &tc.name,
                                                &denied_msg,
                                            );
                                            continue;
                                        }
                                        crate::permissions::PermissionResult::Prompt => {
                                            // Show the Shell call so the user
                                            // has context for what they're
                                            // approving — display is normally
                                            // suppressed inside Update groups.
                                            let display = tools::format_tool_call_display(
                                                &tc.name,
                                                &tc.arguments,
                                            );
                                            println!(
                                                "{} {}",
                                                "●"
                                                    .with(random_bullet_color())
                                                    .attribute(crossterm::style::Attribute::Bold),
                                                display.themed(AuraStyle::Primary),
                                            );
                                            if !permissions
                                                .prompt_tool_permission(&tc.name, &tc.arguments)
                                            {
                                                let reason = "denied by user".to_string();
                                                let rules = permissions.describe_rules();
                                                let denied_msg = tools::permission_denied_message(
                                                    "Shell",
                                                    &reason,
                                                    rules.as_deref(),
                                                );
                                                conversation.add_tool_result(
                                                    &tc.id,
                                                    &tc.name,
                                                    &denied_msg,
                                                );
                                                continue;
                                            }
                                        }
                                        crate::permissions::PermissionResult::Allowed => {}
                                    }

                                    let start = std::time::Instant::now();
                                    let tool_result = tools::execute_tool("Shell", &tc.arguments)
                                        .unwrap_or_else(|e| format!("Error: {e}"));
                                    let duration = start.elapsed();

                                    // Record in the update context
                                    let cmd_name = tools::extract_command_name(&tc.arguments);
                                    let args_val: serde_json::Value =
                                        serde_json::from_str(&tc.arguments).unwrap_or_default();
                                    let full_cmd =
                                        args_val["command"].as_str().unwrap_or("").to_string();

                                    if let Some(ref mut ctx) = active_update {
                                        if !cmd_name.is_empty()
                                            && !ctx.commands_used.contains(&cmd_name)
                                        {
                                            ctx.commands_used.push(cmd_name.clone());
                                        }
                                        ctx.shell_calls.push(ShellCallDetail {
                                            command_name: cmd_name,
                                            full_command: full_cmd,
                                            result: tool_result.clone(),
                                            duration,
                                        });
                                    }

                                    // Add result to conversation (LLM needs feedback)
                                    conversation.add_tool_result(&tc.id, &tc.name, &tool_result);
                                    // Suppress display — grouped under Update
                                    continue;
                                }

                                // --- Non-Update, non-grouped tools ---

                                // If there's an active Update and we hit a non-Shell tool,
                                // finalize the Update first.
                                if let Some(prev) = active_update.take() {
                                    finalize_update(prev, &turn_events);
                                    in_update_group.store(false, Ordering::Relaxed);
                                }

                                // For non-local tools (server-side), use the cached result
                                // from aura.tool_complete events instead of executing locally.
                                // Display was already shown from on_tool_complete callback.
                                if !tools::is_local_tool(&tc.name) {
                                    let result = match server_results.get(&tc.id).cloned() {
                                        Some(r) => r,
                                        None => {
                                            eprintln!(
                                                "{} {}",
                                                "└─".themed(AuraStyle::Warning),
                                                format!(
                                                    "no result for server tool '{}' — server did not stream output (set AURA_CUSTOM_EVENTS=true)",
                                                    tc.name
                                                )
                                                .themed(AuraStyle::Warning),
                                            );
                                            tools::missing_server_result_message(&tc.name)
                                        }
                                    };
                                    conversation.add_tool_result(&tc.id, &tc.name, &result);
                                    continue;
                                }

                                // Show the tool call if permission will be prompted,
                                // so the user has context for what they're approving.
                                let perm = permissions.check(&tc.name, &tc.arguments);
                                if matches!(perm, crate::permissions::PermissionResult::Prompt) {
                                    let display =
                                        tools::format_tool_call_display(&tc.name, &tc.arguments);
                                    println!(
                                        "{} {}",
                                        "●"
                                            .with(random_bullet_color())
                                            .attribute(crossterm::style::Attribute::Bold),
                                        display.themed(AuraStyle::Primary),
                                    );
                                }

                                // Execute the tool (with permission check)
                                let start = std::time::Instant::now();
                                let tool_result = match perm {
                                    crate::permissions::PermissionResult::Allowed => {
                                        tools::execute_tool(&tc.name, &tc.arguments)
                                            .unwrap_or_else(|e| format!("Error: {e}"))
                                    }
                                    crate::permissions::PermissionResult::Denied(reason) => {
                                        eprintln!(
                                            "  {}",
                                            reason.as_str().themed(AuraStyle::Warning)
                                        );
                                        let rules = permissions.describe_rules();
                                        tools::permission_denied_message(
                                            &tc.name,
                                            &reason,
                                            rules.as_deref(),
                                        )
                                    }
                                    crate::permissions::PermissionResult::Prompt => {
                                        if permissions
                                            .prompt_tool_permission(&tc.name, &tc.arguments)
                                        {
                                            tools::execute_tool(&tc.name, &tc.arguments)
                                                .unwrap_or_else(|e| format!("Error: {e}"))
                                        } else {
                                            let reason = "denied by user".to_string();
                                            let rules = permissions.describe_rules();
                                            tools::permission_denied_message(
                                                &tc.name,
                                                &reason,
                                                rules.as_deref(),
                                            )
                                        }
                                    }
                                };
                                let duration = start.elapsed();

                                // Record DisplayEvent for expand/replay
                                let parsed_args: BTreeMap<String, serde_json::Value> =
                                    serde_json::from_str(&tc.arguments).unwrap_or_default();
                                if let Ok(mut events) = turn_events.lock() {
                                    events.push(DisplayEvent::ToolCall {
                                        tool_name: tc.name.clone(),
                                        arguments: parsed_args,
                                        duration,
                                        result: Some(tool_result.clone()),
                                    });
                                }

                                // Collect for grouped summary display
                                let display_name =
                                    tools::extract_tool_display_name(&tc.name, &tc.arguments);
                                batch_tools.push((
                                    tc.name.clone(),
                                    display_name,
                                    tc.arguments.clone(),
                                    duration,
                                ));

                                // Add tool result to conversation history
                                conversation.add_tool_result(&tc.id, &tc.name, &tool_result);
                            }

                            // Print summaries for batch of local tools
                            if !batch_tools.is_empty() {
                                #[allow(clippy::type_complexity)]
                                let mut groups: Vec<(
                                    String,
                                    Vec<String>,
                                    Option<String>,
                                    Option<std::time::Duration>,
                                )> = Vec::new();
                                for (name, display, args, dur) in &batch_tools {
                                    if let Some(group) =
                                        groups.iter_mut().find(|(n, _, _, _)| n == name)
                                    {
                                        group.1.push(display.clone());
                                    } else {
                                        groups.push((
                                            name.clone(),
                                            vec![display.clone()],
                                            Some(args.clone()),
                                            Some(*dur),
                                        ));
                                    }
                                }
                                for (name, displays, first_args, first_duration) in &groups {
                                    if displays.len() == 1 {
                                        let args_str = first_args.as_deref().unwrap_or("{}");
                                        let args_map: std::collections::BTreeMap<
                                            String,
                                            serde_json::Value,
                                        > = serde_json::from_str(args_str).unwrap_or_default();
                                        crate::ui::prompt::print_tool_call_summary(
                                            name,
                                            &args_map,
                                            *first_duration,
                                        );
                                    } else {
                                        // Multiple calls: grouped summary
                                        let header =
                                            tools::format_tool_group_header(name, displays.len());
                                        tools::print_tool_group(&header, displays, false);
                                    }
                                    println!();
                                }
                            }

                            // Restart thinking animation for next iteration
                            let (new_anim, new_stop) = WaveAnimation::start(
                                "Thinking",
                                vec![],
                                input_buf.clone(),
                                Some(cancel_flag.clone()),
                            );
                            // The new animation has its own internal stop flag.
                            // Callbacks will stop it via ptw_anim.finish(), not stop_flag.
                            drop(new_stop);
                            // Store the new animation so it gets cleaned up properly
                            if let Ok(mut guard) = post_tool_wave.lock() {
                                *guard = Some((new_anim, stop_flag.clone()));
                            }
                            prepare_input_line(&input_buf, Some(&cancel_flag));

                            // Continue the tool loop
                            continue 'tool_loop;
                        }
                        Err(e) => {
                            tool_loop_error = Some(e);
                            break 'tool_loop;
                        }
                    }
                }

                // Hide cursor for final rendering; drain any remaining keystrokes
                let _ = execute!(io::stdout(), cursor::Hide);
                if let Ok(mut buf) = input_buf.lock() {
                    drain_stdin(&mut buf);
                }

                // Clear thinking animation if still running
                if !anim_cleared.load(Ordering::Relaxed) {
                    if let Some(a) = anim.take() {
                        a.finish();
                    }
                } else {
                    drop(anim);
                }

                // Stop post-tool thinking animation if still running
                if let Ok(mut guard) = post_tool_wave.lock()
                    && let Some((ptw_anim, _)) = guard.take()
                {
                    ptw_anim.finish();
                }

                // Erase any remaining input frame before printing
                erase_input_frame();

                // Finalize any active Update group when the tool loop ends
                if let Some(ctx) = active_update.take() {
                    finalize_update(ctx, &turn_events);
                    in_update_group.store(false, Ordering::Relaxed);
                }

                // Drain per-turn tool/usage events into the global event log
                if let Ok(mut events) = turn_events.lock() {
                    let drained: Vec<_> = events.drain(..).collect();
                    extend_display_events(drained);
                }

                // If compaction happened this turn, trim event_log to match.
                // We must adjust event_log_turn_start since drain shifts indices.
                let event_log_turn_start = if did_compact {
                    with_event_log_mut(|log| {
                        let half = log.len() / 2;
                        log.drain(..half);
                        // The turn's events were appended at the old tail which is now
                        // shifted back by `half`. Clamp so we don't go negative or OOB.
                        event_log_turn_start.saturating_sub(half).min(log.len())
                    })
                } else {
                    event_log_turn_start
                };

                let was_cancelled = cancel_flag.load(Ordering::Relaxed);

                if was_cancelled {
                    // Display cancellation message in the Thinking style
                    println!(
                        "{} {}",
                        "●"
                            .with(random_bullet_color())
                            .attribute(crossterm::style::Attribute::Bold),
                        "Interrupted (user requested)".attribute(crossterm::style::Attribute::Bold),
                    );
                    println!(
                        "{} {}",
                        "└─".themed(AuraStyle::Connector),
                        "what should AURA do next?".themed(AuraStyle::Muted),
                    );
                    println!();
                    push_display_event(DisplayEvent::Cancelled);
                    // Remove the user message that got no response
                    conversation.pop_last_user();
                } else if let Some(e) = tool_loop_error {
                    println!(
                        "{} {}",
                        "●"
                            .with(random_bullet_color())
                            .attribute(crossterm::style::Attribute::Bold),
                        "Error".themed(AuraStyle::Error),
                    );
                    if crate::api::client::is_model_error(&e) {
                        let hint = match get_selected_model() {
                            Some(m) => format!(
                                "The model \"{}\" is not available. Use /model to select a valid model.",
                                m,
                            ),
                            None => {
                                "No model is configured. Use /model to select a model.".to_string()
                            }
                        };
                        eprintln!(
                            "{} {}",
                            "└─".themed(AuraStyle::Connector),
                            hint.as_str().themed(AuraStyle::Warning),
                        );
                        push_display_event(DisplayEvent::Error(hint.clone()));
                        conversation.add_assistant(&format!("[Error: {}]", hint));
                    } else {
                        eprintln!(
                            "{} {}",
                            "└─".themed(AuraStyle::Connector),
                            format!("{:#}", e).themed(AuraStyle::Warning),
                        );
                        push_display_event(DisplayEvent::Error(format!("{:#}", e)));
                        conversation.add_assistant(&format!("[Error: {}]", e));
                    }
                } else {
                    if !final_text.is_empty() {
                        // Only run the LLM title-gen path for multi-line responses
                        // when the user has opted in. For single-line responses the
                        // bullet line carries the response itself and the body is
                        // empty — no separate title is generated.
                        let is_multi_line = final_text.trim().contains('\n');

                        let (summary, usage, displayed_text) = if is_multi_line
                            && config.enable_final_response_summary
                        {
                            let (summarize_anim, _) = WaveAnimation::start(
                                "Thinking",
                                vec![],
                                input_buf.clone(),
                                Some(cancel_flag.clone()),
                            );
                            prepare_input_line(&input_buf, Some(&cancel_flag));

                            let summary_session_id = crate::api::session::resolve_chat_session_id(
                                &config.extra_headers,
                                conv_store
                                    .as_ref()
                                    .map(|s| s.uuid.as_str())
                                    .unwrap_or(&fallback_session_uuid),
                                crate::api::session::SessionKind::Summary,
                            );
                            let summarize_result =
                                rt.block_on(backend.summarize(&final_text, &summary_session_id));

                            summarize_anim.finish();
                            erase_input_frame();

                            let (s, u) = if cancel_flag.load(Ordering::Relaxed) {
                                ("Response".to_string(), None)
                            } else {
                                summarize_result.unwrap_or(("Response".to_string(), None))
                            };
                            (s, u, final_text.clone())
                        } else {
                            let (first, rest) =
                                crate::api::session::split_first_line_summary(&final_text);
                            (first, None, rest)
                        };

                        if let Some((prompt_tokens, completion_tokens)) = usage {
                            set_status_bar_tokens(prompt_tokens, completion_tokens);
                            push_display_event(DisplayEvent::Usage {
                                prompt_tokens,
                                completion_tokens,
                            });
                        }

                        push_display_event(DisplayEvent::AssistantResponse {
                            summary: summary.clone(),
                            text: displayed_text.clone(),
                        });

                        if !summary.is_empty() {
                            println!(
                                "{} {}",
                                "●"
                                    .with(random_bullet_color())
                                    .attribute(crossterm::style::Attribute::Bold),
                                summary.attribute(crossterm::style::Attribute::Bold),
                            );
                        }
                        if !displayed_text.is_empty() {
                            println!();
                            render_markdown(&displayed_text);
                        }
                        println!();
                    }

                    conversation.add_assistant(&final_text);

                    // Check if we've crossed a 2M token boundary — nudge on next turn
                    let current_tokens = get_cumulative_tokens();
                    if current_tokens >= last_compact_prompt_threshold {
                        while last_compact_prompt_threshold <= current_tokens {
                            last_compact_prompt_threshold += 2_000_000;
                        }
                        compact_hint_pending = true;
                        // Show "Context left: N%" in the status bar from now on
                        crate::ui::prompt::set_auto_compact_ceiling(8_000_000);
                    }
                }

                // Auto-save conversation after each turn
                if let Some(ref store) = conv_store {
                    let expanded = is_expanded_output();
                    with_event_log(|log| {
                        store.save_all(conversation.messages(), log, expanded);
                        // Append new usage events from this turn to the usage JSONL
                        for event in &log[event_log_turn_start..] {
                            if let DisplayEvent::Usage {
                                prompt_tokens,
                                completion_tokens,
                            } = event
                            {
                                store.append_usage(
                                    *prompt_tokens,
                                    *completion_tokens,
                                    get_selected_model().as_deref(),
                                );
                            }
                        }
                    });
                }

                // Drain any chars typed during rendering, then check for queued input
                if let Ok(mut buf) = input_buf.lock() {
                    drain_stdin(&mut buf);
                }
                let queued = take_queued_input();

                if !queued.is_empty() {
                    if was_cancelled || compact_hint_pending {
                        // Pre-fill readline so user can confirm/edit
                        // (also don't auto-submit when compact hint is pending,
                        // so the user can see the LLM's response first)
                        initial_input = queued;
                        auto_submit = false;
                    } else {
                        // Auto-submit the queued input
                        initial_input = queued;
                        auto_submit = true;
                    }
                    // Discard any leftover un-submitted chars
                    if let Ok(mut buf) = input_buf.lock() {
                        buf.clear();
                    }
                } else {
                    // No queued input — capture leftover chars for readline
                    initial_input = input_buf.lock().map(|b| b.clone()).unwrap_or_default();
                }

                // Done processing — restore status bar to token counts / default
                set_processing(false);

                // Check for pending commands set by mid-stream slash commands
                if let Some(cmd) = take_pending_command() {
                    match cmd {
                        PendingCommand::Quit => {
                            // Save before exiting, or delete if conversation was never started
                            if let Some(ref store) = conv_store {
                                if conversation.messages().len() > 1 {
                                    with_event_log(|log| {
                                        store.save_all(
                                            conversation.messages(),
                                            log,
                                            is_expanded_output(),
                                        )
                                    });
                                } else {
                                    store.delete();
                                }
                            }
                            break;
                        }
                        PendingCommand::Clear => {
                            commands::handle_clear(
                                &mut conversation,
                                &mut conv_store,
                                &mut input_reader,
                            );
                            continue;
                        }
                        PendingCommand::Resume(filter) => {
                            let arg = filter.trim();
                            if arg.is_empty() {
                                // No argument — just continue normally
                                redraw_input_frame();
                                continue;
                            }
                            // In-REPL resume ignores CLI --model and --system-prompt
                            if let Some(new_input) = commands::handle_resume(
                                arg,
                                &mut conversation,
                                &mut conv_store,
                                &mut input_reader,
                                None,
                            ) {
                                initial_input = new_input;
                            }
                            redraw_input_frame();
                            continue;
                        }
                    }
                }

                // Frame is the last thing in the output
                redraw_input_frame();
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D — exit immediately
                let _ = execute!(io::stdout(), cursor::MoveUp(1));
                break;
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C — require double-press within 5 s to quit
                if handle_ctrlc() {
                    let _ = execute!(io::stdout(), cursor::MoveUp(1));
                    break;
                }
                // First press: erase old frame and redraw with hint visible.
                // After Interrupted, rustyline left cursor one row below the
                // text end (same as Enter). Navigate to the frame's last input
                // row so erase_input_frame's contract is satisfied.
                let _ = execute!(io::stdout(), cursor::Hide);
                let fl = frame_lines() as i32;
                let tl = text_lines() as i32;
                let move_to_frame_end = (fl - 1) - tl;
                if move_to_frame_end >= 0 {
                    let _ = execute!(io::stdout(), cursor::MoveDown(move_to_frame_end as u16));
                } else {
                    let _ = execute!(io::stdout(), cursor::MoveUp((-move_to_frame_end) as u16));
                }
                erase_input_frame();
                redraw_input_frame();
                continue;
            }
            Err(err) => {
                let _ = execute!(io::stdout(), cursor::Hide);
                let _ = execute!(io::stdout(), cursor::MoveUp(1));
                erase_input_frame();
                eprintln!("Input error: {}", err);
                break;
            }
        }
    }

    // Save conversation on exit, or delete if conversation was never started
    if let Some(ref store) = conv_store {
        if conversation.messages().len() > 1 {
            with_event_log(|log| {
                store.save_all(conversation.messages(), log, is_expanded_output())
            });
        } else {
            store.delete();
        }
        // Save any partially-typed input so it can be restored on resume
        let pending = LAST_READLINE_INPUT
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        store.save_pending_input(&pending);
    }

    if let Some(ref path) = history_path {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = input_reader.save_history(path);
    }

    erase_input_frame();
    cleanup_terminal();

    // Print resume hint if the conversation had content
    if let Some(ref store) = conv_store
        && conversation.messages().len() > 1
    {
        let short_id = &store.uuid[..8.min(store.uuid.len())];
        println!(
            "Resume this conversation with: {} {}",
            "aura-cli --resume".themed(AuraStyle::Identifier),
            short_id.themed(AuraStyle::Identifier),
        );
    }

    println!();
    Ok(())
}

/// Streaming event handler for the interactive REPL: drives the live
/// "Thinking" animation, tool-call rendering, reasoning blocks, and the
/// orchestrator task tree. State is shared via `Arc` so each turn's stream
/// mutates the same terminal-display state the REPL reads between turns.
struct ReplStreamHandler {
    pending_args:
        Arc<Mutex<std::collections::HashMap<String, BTreeMap<String, serde_json::Value>>>>,
    turn_events: Arc<Mutex<Vec<DisplayEvent>>>,
    live_reasoning: Arc<Mutex<Option<LiveReasoning>>>,
    stop_flag: Arc<AtomicBool>,
    anim_cleared: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    #[allow(clippy::type_complexity)]
    post_tool_wave: Arc<Mutex<Option<(WaveAnimation, Arc<AtomicBool>)>>>,
    input_buf: Arc<Mutex<String>>,
    orch_state: Arc<Mutex<OrchDisplayState>>,
    needs_blank: Arc<AtomicBool>,
    in_update_group: Arc<AtomicBool>,
}

impl StreamHandler for ReplStreamHandler {
    // on_token / on_tool_start stay no-ops (trait defaults): text is
    // accumulated silently and the animation keeps running until a later
    // event needs to display.

    fn on_tool_requested(
        &mut self,
        tool_id: &str,
        tool_name: &str,
        args: &BTreeMap<String, serde_json::Value>,
    ) {
        // Record args for later pairing with on_tool_complete
        if let Ok(mut map) = self.pending_args.lock() {
            map.insert(tool_id.to_string(), args.clone());
        }
        // Track Update grouping flag for the execution loop
        if tool_name == "Update" {
            self.in_update_group.store(true, Ordering::Relaxed);
        } else if tool_name != "Shell" || !self.in_update_group.load(Ordering::Relaxed) {
            // Non-Shell (or Shell outside Update) clears the flag
            if tool_name != "Shell" {
                self.in_update_group.store(false, Ordering::Relaxed);
            }
        }
    }

    fn on_tool_complete(
        &mut self,
        tool_id: &str,
        tool_name: &str,
        duration: std::time::Duration,
        result: Option<&str>,
    ) {
        flush_live_reasoning(&self.live_reasoning);
        // Stop animation: prefer ptw (later iterations), fall back
        // to initial animation (first iteration).
        let had_ptw = if let Ok(mut guard) = self.post_tool_wave.lock() {
            if let Some((ptw_anim, _)) = guard.take() {
                ptw_anim.finish();
                true
            } else {
                false
            }
        } else {
            false
        };
        if !had_ptw && !self.anim_cleared.load(Ordering::Relaxed) {
            stop_and_clear_animation(&self.stop_flag);
            self.anim_cleared.store(true, Ordering::Relaxed);
        }
        // Show server-side tool result
        {
            let _term = lock_term();
            erase_input_frame();
            let args = self
                .pending_args
                .lock()
                .ok()
                .and_then(|mut map| map.remove(tool_id))
                .unwrap_or_default();
            if is_expanded_output() {
                print_tool_call_expanded(tool_name, &args, duration, result);
            } else {
                crate::ui::prompt::print_tool_call_summary(tool_name, &args, Some(duration));
                println!();
            }
            if let Ok(mut events) = self.turn_events.lock() {
                events.push(DisplayEvent::ToolCall {
                    tool_name: tool_name.to_string(),
                    arguments: args,
                    duration,
                    result: result.map(|s| s.to_string()),
                });
            }
        }
        // Start "Thinking" animation to fill gap until next event
        let (wave_anim, wave_stop) = {
            let _term = lock_term();
            WaveAnimation::start(
                "Thinking",
                vec![],
                self.input_buf.clone(),
                Some(self.cancel.clone()),
            )
        };
        if let Ok(mut guard) = self.post_tool_wave.lock() {
            *guard = Some((wave_anim, wave_stop));
        }
        prepare_input_line(&self.input_buf, Some(&self.cancel));
    }

    fn on_usage(&mut self, prompt_tokens: u64, completion_tokens: u64) {
        set_status_bar_tokens(prompt_tokens, completion_tokens);
        update_status_bar();
        if let Ok(mut events) = self.turn_events.lock() {
            events.push(DisplayEvent::Usage {
                prompt_tokens,
                completion_tokens,
            });
        }
    }

    fn on_reasoning(
        &mut self,
        content: &str,
        agent_id: &str,
        fields: &BTreeMap<String, serde_json::Value>,
    ) {
        if content.is_empty() {
            return;
        }

        let parent_agent_id = fields
            .get("parent_agent_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Switching agents closes the previous block.
        let agent_changed = match self.live_reasoning.lock() {
            Ok(g) => g.as_ref().is_some_and(|s| s.agent_id != agent_id),
            Err(_) => false,
        };
        if agent_changed {
            flush_live_reasoning(&self.live_reasoning);
        }

        // Initialize the live block on the first chunk.
        let need_init = match self.live_reasoning.lock() {
            Ok(g) => g.is_none(),
            Err(_) => return,
        };
        if need_init {
            let is_worker = parent_agent_id.is_some();
            let task_id = if is_worker {
                self.orch_state.lock().ok().and_then(|os| {
                    os.tasks
                        .iter()
                        .find(|(_, info)| info.worker_id == agent_id)
                        .map(|(tid, _)| tid.clone())
                })
            } else {
                None
            };
            // No matching task means we can't insert
            // into the tree — fall back to top-level
            // rendering so the reasoning isn't lost.
            let effective_worker = is_worker && task_id.is_some();

            // Stop the running animation and erase
            // the input frame BEFORE printing any
            // new scrollback rows — both top-level
            // (`open_top_level_reasoning_block`) and
            // worker (`open_worker_reasoning_block`
            // → `register_orch_reasoning_in_tree`)
            // emit `println!` rows. If we skip this,
            // the println lands at the InputLine
            // and the old anim/frame rows above are
            // left stale in scrollback (the doubled
            // "● Thinking" + duplicate body bug).
            let had_ptw = if let Ok(mut guard) = self.post_tool_wave.lock() {
                if let Some((ptw_anim, _)) = guard.take() {
                    ptw_anim.finish();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !had_ptw && !self.anim_cleared.load(Ordering::Relaxed) {
                stop_and_clear_animation(&self.stop_flag);
                self.anim_cleared.store(true, Ordering::Relaxed);
            }

            let mut new_state = LiveReasoning {
                agent_id: agent_id.to_string(),
                parent_agent_id: parent_agent_id.clone(),
                task_id: task_id.clone(),
                combined: String::new(),
                fields: fields.clone(),
                started: false,
                body_line_num: 0,
                body_line_text: String::new(),
                body_visual_rows: 1,
                is_worker: effective_worker,
            };
            {
                let _term = lock_term();
                erase_input_frame();
                if effective_worker {
                    let tid = task_id.as_deref().unwrap_or("");
                    open_worker_reasoning_block(&mut new_state, tid);
                } else {
                    open_top_level_reasoning_block(&mut new_state, agent_id);
                }
            }
            // Restart the Thinking spinner so it
            // stays visible while reasoning chunks
            // stream in. `WaveAnimation::start`
            // prints the anim overlay BELOW the
            // body row, so the `MoveUp(3)` tick and
            // the `(total_sb + 3) - body_line_num`
            // in-place body rewrite both target
            // rows below the body — body is safe.
            let (wave_anim, wave_stop) = {
                let _term = lock_term();
                WaveAnimation::start(
                    "Thinking",
                    vec![],
                    self.input_buf.clone(),
                    Some(self.cancel.clone()),
                )
            };
            if let Ok(mut guard) = self.post_tool_wave.lock() {
                *guard = Some((wave_anim, wave_stop));
            }
            prepare_input_line(&self.input_buf, Some(&self.cancel));
            // Animation is live again — the orch
            // handler's "stop if not cleared"
            // check needs to fire on the next
            // event to finish this wave.
            self.anim_cleared
                .store(false, std::sync::atomic::Ordering::Relaxed);
            if let Ok(mut guard) = self.live_reasoning.lock() {
                *guard = Some(new_state);
            }
        }

        // For top-level reasoning, decide whether
        // this chunk grows the body row-count
        // BEFORE we update state — extension needs
        // to release the live_reasoning lock so
        // the animation halt+restart can re-acquire
        // it safely.
        let extend_by: u32 = {
            if let Ok(guard) = self.live_reasoning.lock() {
                if let Some(state) = guard.as_ref() {
                    if state.is_worker {
                        0
                    } else {
                        let mut candidate = state.body_line_text.clone();
                        candidate.push_str(content);
                        let lines = top_level_reasoning_body_for_terminal(&candidate);
                        let required = lines.len() as u32;
                        required.saturating_sub(state.body_visual_rows)
                    }
                } else {
                    0
                }
            } else {
                0
            }
        };

        if extend_by > 0 {
            // Halt anim, extend scrollback by
            // `extend_by` rows (overwriting the
            // current trailing-blank separator with
            // body content + appending a new
            // separator at the end), restart anim.
            // Same halt-restart shape as
            // `tool_call_started`; spinner only
            // pauses for the extension itself, not
            // for the chunk.
            let had_ptw = if let Ok(mut guard) = self.post_tool_wave.lock() {
                if let Some((ptw_anim, _)) = guard.take() {
                    ptw_anim.finish();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !had_ptw && !self.anim_cleared.load(Ordering::Relaxed) {
                stop_and_clear_animation(&self.stop_flag);
                self.anim_cleared.store(true, Ordering::Relaxed);
            }
            {
                let _term = lock_term();
                erase_input_frame();
                // After `erase_input_frame`, the
                // cursor sits where the frame's
                // top border was — exactly one row
                // below the existing trailing
                // separator. Println `extend_by`
                // blank rows from here: each
                // println commits one new
                // scrollback row below the prior
                // separator. The old separator
                // (still blank at its row) becomes
                // the first new body row when
                // `rewrite_top_level_reasoning_body_inplace`
                // fills the expanded body region;
                // the LAST println becomes the
                // new trailing separator.
                //
                // Do NOT `MoveUp(1) + Clear` here:
                // that overwrites the existing
                // separator row WITHOUT committing
                // a new scrollback row, but still
                // increments the counter, leaving
                // it ahead by 1 and making the
                // next body rewrite's `MoveUp`
                // land on the header.
                for _ in 0..extend_by {
                    println!();
                    crate::ui::prompt::increment_orch_scrollback();
                }
            }
            let (wave_anim, wave_stop) = {
                let _term = lock_term();
                WaveAnimation::start(
                    "Thinking",
                    vec![],
                    self.input_buf.clone(),
                    Some(self.cancel.clone()),
                )
            };
            if let Ok(mut guard) = self.post_tool_wave.lock() {
                *guard = Some((wave_anim, wave_stop));
            }
            prepare_input_line(&self.input_buf, Some(&self.cancel));
            self.anim_cleared
                .store(false, std::sync::atomic::Ordering::Relaxed);

            if let Ok(mut guard) = self.live_reasoning.lock()
                && let Some(s) = guard.as_mut()
            {
                s.body_visual_rows += extend_by;
            }
        }

        if let Ok(mut guard) = self.live_reasoning.lock() {
            let Some(state) = guard.as_mut() else {
                return;
            };
            let _term = lock_term();
            apply_reasoning_chunk(state, content);
            if state.is_worker {
                // Keep `ORCH_LAST_TOOL_LINES` in sync
                // so the next tool's upgrade redraws
                // the body with the latest content.
                if let Some(tid) = state.task_id.clone() {
                    let body = worker_reasoning_body_for_terminal(&state.body_line_text);
                    crate::ui::orchestrator::update_orch_last_tool_duration_text(&tid, &body);
                }
            }
        }
    }

    fn on_raw_event(&mut self, event_name: &str, event_data: &str) {
        push_sse_event(event_name, event_data);
        if event_name == event_names::TOOL_START
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(event_data)
            && let Some(tool_id) = val.get("tool_id").and_then(|v| v.as_str())
            && let Some(token) = val.get("progress_token")
            && !token.is_null()
        {
            crate::ui::prompt::record_tool_progress_token(tool_id, &token.to_string());
        }
    }

    fn on_orchestrator_event(&mut self, event_name: &str, val: &serde_json::Value) {
        // Handle aura.progress — prefer to attach the
        // message to the active orchestrator tool whose
        // progress_token matches; fall back to the
        // global "Thinking" sub-line when there's no
        // active tool to attach it to (single-agent
        // mode, or progress for a tool we never saw a
        // tool_start for).
        //
        // These transient events (progress, worker_phase,
        // session_info, non-orchestrator-prefixed) update
        // the spinner sub-line or task header in-place —
        // they don't add to scrollback, so they must NOT
        // flush the live reasoning block. Flushing here
        // would slice multi-chunk reasoning into multiple
        // `● Reasoning - <agent>` blocks every time a
        // worker phase changes.
        if event_name == event_names::PROGRESS {
            if let Some(message) = val.get("message").and_then(|v| v.as_str()) {
                let token = val.get("progress_token").map(|t| t.to_string());
                let attached = token
                    .as_deref()
                    .map(|t| crate::ui::prompt::set_orch_tool_progress_by_token(t, message))
                    .unwrap_or(false);
                if !attached {
                    crate::ui::prompt::set_agent_reasoning(message);
                }
            }
            return;
        }

        // Reflect the worker's current phase
        // (`Planning`/`Executing`/`Analyzing`) in the
        // task header. Overwrites in-place; the
        // task_completed handler later replaces this
        // with `done`.
        if event_name == event_names::WORKER_PHASE {
            let task_id = val.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let phase = val.get("phase").and_then(|v| v.as_str()).unwrap_or("");
            if task_id.is_empty() || phase.is_empty() {
                return;
            }
            let mut chars = phase.chars();
            let phase_label: String = match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect(),
                None => return,
            };
            let header_info = if let Ok(os) = self.orch_state.lock() {
                os.tasks
                    .get(task_id)
                    .map(|t| (t.header_line_num, t.worker_id.clone()))
            } else {
                None
            };
            if let Some((header_line, worker_id)) = header_info {
                crate::ui::prompt::overwrite_orch_task_header(
                    header_line,
                    task_id,
                    &worker_id,
                    &phase_label,
                );
            }
            return;
        }

        // Scratchpad savings ride in on the base `aura.scratchpad_usage`
        // event (not orchestrator-namespaced), so handle it here, before the
        // orchestrator-prefix guard below. Accumulate into the status bar and
        // record a DisplayEvent for history replay.
        if event_name == event_names::SCRATCHPAD_USAGE {
            let tokens_intercepted = val
                .get("tokens_intercepted")
                .or_else(|| val.get("data").and_then(|d| d.get("tokens_intercepted")))
                .and_then(|f| f.as_u64())
                .unwrap_or(0);
            let tokens_extracted = val
                .get("tokens_extracted")
                .or_else(|| val.get("data").and_then(|d| d.get("tokens_extracted")))
                .and_then(|f| f.as_u64())
                .unwrap_or(0);
            crate::ui::prompt::add_scratchpad_usage(tokens_intercepted, tokens_extracted);
            update_status_bar_unlocked();
            push_display_event(DisplayEvent::OrchestratorScratchpadSavings {
                tokens_intercepted,
                tokens_extracted,
            });
            return;
        }

        // Everything below acts only on `aura.orchestrator.*` events; bail
        // out for anything else (e.g. session_info).
        if !event_name.starts_with("aura.orchestrator.") {
            return;
        }

        // The server emits `aura.orchestrator.worker_reasoning`
        // alongside every `aura.reasoning` delta from a
        // worker. We already render the reasoning via
        // the dedicated `on_reasoning` path (which has
        // the worker's agent_id and richer correlation
        // context), and processing the orch mirror here
        // would call `erase_input_frame` mid-stream and
        // corrupt the running `● Reasoning - <agent>`
        // block. Drop it before any frame work runs.
        if event_name == event_names::WORKER_REASONING {
            return;
        }

        // Non-printable orch subs (synthesizing and any
        // future no-print subs) need their side effects but
        // must NOT disturb the input frame mid-reasoning.
        // Handle them inline and bail out before the
        // `erase_input_frame` machinery runs.
        if !orch_event_prints_scrollback(event_name) {
            if event_name == event_names::SYNTHESIZING {
                crate::ui::prompt::clear_agent_reasoning();
                push_display_event(DisplayEvent::OrchestratorSynthesizing);
            }
            return;
        }

        // Past this point: event_name is in
        // `orch_event_prints_scrollback`, so close the
        // live reasoning block before the event lands.
        // The block's trailing blank (committed in
        // `open_top_level_reasoning_block`) is the
        // separator — no extra println needed here.
        let _ = flush_live_reasoning(&self.live_reasoning);

        // Stop current animation
        let had_ptw = if let Ok(mut guard) = self.post_tool_wave.lock() {
            if let Some((ptw_anim, _)) = guard.take() {
                ptw_anim.finish();
                true
            } else {
                false
            }
        } else {
            false
        };
        if !had_ptw && !self.anim_cleared.load(Ordering::Relaxed) {
            stop_and_clear_animation(&self.stop_flag);
            self.anim_cleared.store(true, Ordering::Relaxed);
        }
        {
            let _term = lock_term();
            erase_input_frame();

            // Emit a deferred blank line from a previous
            // tool_call_completed — but only if this event
            // isn't another tool starting directly after.
            if self.needs_blank.swap(false, Ordering::Relaxed)
                && event_name != event_names::TOOL_CALL_STARTED
            {
                println!();
                crate::ui::prompt::increment_orch_scrollback();
            }

            // Helpers
            let get_str = |v: &serde_json::Value, field: &str| -> String {
                let f = v
                    .get(field)
                    .or_else(|| v.get("data").and_then(|d| d.get(field)));
                match f {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    Some(serde_json::Value::Bool(b)) => b.to_string(),
                    _ => String::new(),
                }
            };
            let get_u64 = |v: &serde_json::Value, field: &str| -> u64 {
                v.get(field)
                    .or_else(|| v.get("data").and_then(|d| d.get(field)))
                    .and_then(|f| f.as_u64())
                    .unwrap_or(0)
            };
            let fields: BTreeMap<String, serde_json::Value> = match val.as_object() {
                Some(obj) => obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                None => BTreeMap::new(),
            };
            let parse_args =
                |v: &serde_json::Value| -> Option<serde_json::Map<String, serde_json::Value>> {
                    v.get("arguments")
                        .or_else(|| v.get("data").and_then(|d| d.get("arguments")))
                        .and_then(|a| match a {
                            serde_json::Value::Object(obj) => Some(obj.clone()),
                            serde_json::Value::String(s) => serde_json::from_str(s).ok(),
                            _ => None,
                        })
                };

            // Per-orchestrator-entity bullet color. Keyed by task_id
            // (or `__orchestrator__` for coordinator-level events:
            // plan_created, synthesizing, iteration_complete).
            // Resolved through the active theme — switching themes
            // repaints automatically; no colors are persisted.
            let event_task_id = get_str(val, "task_id");
            let color_key: String = if !event_task_id.is_empty() {
                event_task_id.clone()
            } else {
                "__orchestrator__".to_string()
            };
            let bullet_color = task_color_for(&color_key);

            match event_name {
                event_names::PLAN_CREATED => {
                    let goal = get_str(val, "goal");
                    // New plan resets reasoning
                    crate::ui::prompt::clear_agent_reasoning();
                    println!(
                        "{} {}",
                        "●"
                            .with(bullet_color)
                            .attribute(crossterm::style::Attribute::Bold),
                        format!("Plan - {goal}").attribute(crossterm::style::Attribute::Bold),
                    );
                    crate::ui::prompt::increment_orch_scrollback();
                    if is_expanded_output() {
                        // Count lines that print_fields_tree will emit
                        let field_lines = fields
                            .values()
                            .map(|v| {
                                if let serde_json::Value::Object(obj) = v {
                                    1 + obj.len()
                                } else {
                                    1
                                }
                            })
                            .sum::<usize>();
                        print_fields_tree(&fields);
                        for _ in 0..field_lines {
                            crate::ui::prompt::increment_orch_scrollback();
                        }
                    }
                    println!();
                    crate::ui::prompt::increment_orch_scrollback();
                    push_display_event(DisplayEvent::OrchestratorPlanCreated { goal, fields });
                }
                event_names::TASK_STARTED => {
                    let worker_id = get_str(val, "worker_id");
                    let task_id = get_str(val, "task_id");
                    let description = get_str(val, "description");
                    // Bullet color resolved per-task at render time —
                    // `task_color_for(&task_id)` returns the color the
                    // generic block (above) already computed via
                    // `bullet_color`, so reuse that directly.
                    crate::ui::prompt::set_agent_reasoning(&description);
                    let header_line = crate::ui::prompt::current_orch_scrollback();
                    println!(
                        "{} {} {} {}",
                        "●"
                            .with(bullet_color)
                            .attribute(crossterm::style::Attribute::Bold),
                        format!("Task {task_id}").attribute(crossterm::style::Attribute::Bold),
                        "-".themed(AuraStyle::Muted),
                        format!("Worker: {worker_id}").themed(AuraStyle::Muted),
                    );
                    crate::ui::prompt::increment_orch_scrollback();
                    // No blank line – tool calls follow directly
                    if let Ok(mut os) = self.orch_state.lock() {
                        os.tasks.insert(
                            task_id.clone(),
                            OrchTaskInfo {
                                header_line_num: header_line,
                                worker_id: worker_id.clone(),
                                tools: Vec::new(),
                            },
                        );
                    }
                    push_display_event(DisplayEvent::OrchestratorTaskStarted {
                        worker_id,
                        task_id,
                        description,
                        fields,
                    });
                }
                event_names::TOOL_CALL_STARTED => {
                    let tool_name = get_str(val, "tool_name");
                    let tool_initiator_id = get_str(val, "tool_initiator_id");
                    let tool_call_id = get_str(val, "tool_call_id");
                    let task_id_str = get_str(val, "task_id");
                    let display_name = snake_to_pascal_case(&tool_name);
                    let args_obj = parse_args(val);
                    let args_summary = args_obj.as_ref()
                                            .map(|obj| {
                                                obj.iter()
                                                    .filter(|(k, v)| {
                                                        !k.starts_with('_')
                                                            && !matches!(v, serde_json::Value::Null)
                                                            && !matches!(v, serde_json::Value::String(s) if s.is_empty() || s == "null")
                                                    })
                                                    .take(3)
                                                    .map(|(k, v)| {
                                                        let val_str = match v {
                                                            serde_json::Value::String(s) => {
                                                                if s.chars().count() > 20 {
                                                                    let prefix: String = s.chars().take(17).collect();
                                                                    format!("\"{prefix}...\"")
                                                                } else {
                                                                    format!("\"{s}\"")
                                                                }
                                                            }
                                                            other => {
                                                                let s = other.to_string();
                                                                if s.chars().count() > 20 {
                                                                    let prefix: String = s.chars().take(17).collect();
                                                                    format!("{prefix}...")
                                                                } else {
                                                                    s
                                                                }
                                                            }
                                                        };
                                                        format!("{k}: {val_str}")
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join(", ")
                                            })
                                            .unwrap_or_default();
                    let reasoning = args_obj
                        .as_ref()
                        .and_then(|obj| obj.get("_aura_reasoning").and_then(|v| v.as_str()));
                    // Set reasoning in the Thinking animation sub-line
                    if let Some(text) = reasoning {
                        crate::ui::prompt::set_agent_reasoning(text);
                    }
                    // Print tool line indented under the task with live duration
                    let tool_display = format!("{display_name}({args_summary})");
                    // Use tool_call_id as primary key, fall back to tool_initiator_id
                    let match_id = if !tool_call_id.is_empty() {
                        &tool_call_id
                    } else {
                        &tool_initiator_id
                    };
                    crate::ui::prompt::register_orch_tool(
                        match_id,
                        &task_id_str,
                        &tool_display,
                        std::time::Instant::now(),
                        &fields,
                    );
                    // Track tool under its task
                    if let Ok(mut os) = self.orch_state.lock() {
                        if let Some(task) = os.tasks.get_mut(&task_id_str) {
                            task.tools.push(tool_display);
                        } else if let Some(task) = os.tasks.get_mut(&tool_initiator_id) {
                            task.tools.push(tool_display);
                        }
                    }
                    push_display_event(DisplayEvent::OrchestratorToolCallStarted {
                        tool_name,
                        tool_initiator_id,
                        fields,
                    });
                }
                event_names::TOOL_CALL_COMPLETED => {
                    let tool_name = get_str(val, "tool_name");
                    let tool_initiator_id = get_str(val, "tool_initiator_id");
                    let tool_call_id = get_str(val, "tool_call_id");
                    let duration_ms_val = val
                        .get("duration_ms")
                        .or_else(|| val.get("data").and_then(|d| d.get("duration_ms")))
                        .and_then(|v| v.as_u64());
                    let result_text = val
                        .get("result")
                        .or_else(|| val.get("data").and_then(|d| d.get("result")))
                        .and_then(|v| v.as_str());
                    // Use tool_call_id as primary key, fall back to tool_initiator_id
                    let match_id = if !tool_call_id.is_empty() {
                        &tool_call_id
                    } else {
                        &tool_initiator_id
                    };
                    // Finalize tool display (solid color + completed duration + live result lines in /expand)
                    crate::ui::prompt::finalize_orch_tool(match_id, duration_ms_val, result_text);
                    self.needs_blank.store(true, Ordering::Relaxed);
                    push_display_event(DisplayEvent::OrchestratorToolCallCompleted {
                        tool_name,
                        tool_initiator_id,
                        duration_ms: duration_ms_val,
                        fields,
                    });
                }
                event_names::TASK_COMPLETED => {
                    let worker_id = get_str(val, "worker_id");
                    let task_id = get_str(val, "task_id");
                    let result = get_str(val, "result");
                    // Look up task info and overwrite the header line in-place
                    let task_info = if let Ok(mut os) = self.orch_state.lock() {
                        os.tasks.remove(&task_id)
                    } else {
                        None
                    };
                    if let Some(info) = &task_info {
                        overwrite_orch_task_header_unlocked(
                            info.header_line_num,
                            &task_id,
                            &worker_id,
                            "done",
                        );
                    }
                    crate::ui::prompt::clear_orch_task_tools(&task_id);
                    // Blank line to separate from next task
                    println!();
                    crate::ui::prompt::increment_orch_scrollback();
                    push_display_event(DisplayEvent::OrchestratorTaskCompleted {
                        worker_id,
                        task_id,
                        result,
                        fields,
                    });
                }
                // `synthesizing` / `scratchpad_usage` /
                // `worker_reasoning` are handled above
                // the frame machinery so they don't
                // disturb in-flight reasoning blocks.
                event_names::ITERATION_COMPLETE => {
                    let iteration = get_u64(val, "iteration");
                    let quality_score = get_str(val, "quality_score");
                    let expanded = is_expanded_output();
                    let has_fields = expanded && !fields.is_empty();
                    crate::ui::prompt::clear_agent_reasoning();
                    println!(
                        "{} {}",
                        "●"
                            .with(bullet_color)
                            .attribute(crossterm::style::Attribute::Bold),
                        "Iteration complete".attribute(crossterm::style::Attribute::Bold),
                    );
                    crate::ui::prompt::increment_orch_scrollback();
                    println!(
                        "{} iteration: {}",
                        "├─".themed(AuraStyle::Connector),
                        iteration.to_string().as_str().themed(AuraStyle::Muted),
                    );
                    crate::ui::prompt::increment_orch_scrollback();
                    let quality_connector = if has_fields { "├─" } else { "└─" };
                    println!(
                        "{} quality: {}",
                        quality_connector.themed(AuraStyle::Connector),
                        quality_score.as_str().themed(AuraStyle::Muted),
                    );
                    crate::ui::prompt::increment_orch_scrollback();
                    if has_fields {
                        let field_lines = fields
                            .values()
                            .map(|v| {
                                if let serde_json::Value::Object(obj) = v {
                                    1 + obj.len()
                                } else {
                                    1
                                }
                            })
                            .sum::<usize>();
                        print_fields_tree(&fields);
                        for _ in 0..field_lines {
                            crate::ui::prompt::increment_orch_scrollback();
                        }
                    }
                    println!();
                    crate::ui::prompt::increment_orch_scrollback();
                    push_display_event(DisplayEvent::OrchestratorIterationComplete {
                        iteration,
                        quality_score,
                        fields,
                    });
                }
                _ => {}
            }
        } // drop _term lock

        // Restart animation
        let anim_label = if event_name == event_names::SYNTHESIZING {
            "Synthesizing"
        } else {
            "Thinking"
        };
        let (wave_anim, wave_stop) = {
            let _term = lock_term();
            WaveAnimation::start(
                anim_label,
                vec![],
                self.input_buf.clone(),
                Some(self.cancel.clone()),
            )
        };
        if let Ok(mut guard) = self.post_tool_wave.lock() {
            *guard = Some((wave_anim, wave_stop));
        }
        prepare_input_line(&self.input_buf, Some(&self.cancel));
    }
}
