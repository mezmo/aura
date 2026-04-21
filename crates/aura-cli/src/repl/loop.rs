use anyhow::Result;
use crossterm::cursor;
use crossterm::execute;
use crossterm::style::Stylize;
use rustyline::error::ReadlineError;
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::runtime::Runtime;

use crate::api::stream::StreamResult;
use crate::api::types::{DisplayEvent, ShellCallDetail, ToolCallInfo, snake_to_pascal_case};
use crate::backend::Backend;
use crate::config::AppConfig;
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
    styled_prompt, take_pending_command, take_queued_input, text_lines, update_status_bar,
    update_status_bar_unlocked, with_event_log, with_event_log_mut,
};
use crate::ui::welcome::WelcomeState;

pub fn run_repl(
    config: AppConfig,
    mut permissions: crate::permissions::PermissionChecker,
    backend: &Backend,
    post_launch_warning: Option<String>,
) -> Result<()> {
    let rt = Runtime::new()?;
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
    print_welcome_state_animated();
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
            "Resumed conversation. Continue below.".with(crossterm::style::Color::Green),
        );
        println!();
        redraw_input_frame();
    }

    // Show post-launch warning if any (A2, C1, C2, C3 scenarios)
    if let Some(ref warning) = post_launch_warning {
        erase_input_frame();
        println!(
            "{}",
            format!("Warning: {}", warning).with(crossterm::style::Color::Yellow),
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
                let pending_args_for_tool = pending_args.clone();
                let pending_args_for_complete = pending_args.clone();
                let turn_events_for_complete = turn_events.clone();
                let turn_events_for_usage = turn_events.clone();
                let cancel_flag = Arc::new(AtomicBool::new(false));

                let (anim, stop_flag) = WaveAnimation::start(
                    "Thinking",
                    vec![],
                    input_buf.clone(),
                    Some(cancel_flag.clone()),
                );
                let mut anim = Some(anim);
                prepare_input_line(&input_buf, Some(&cancel_flag));

                let anim_cleared = Arc::new(AtomicBool::new(false));
                let anim_cleared_for_complete = anim_cleared.clone();
                let anim_cleared_for_orch = anim_cleared.clone();
                let stop_flag_for_token = stop_flag.clone();
                let stop_flag_for_orch = stop_flag.clone();

                let cancel_for_complete = cancel_flag.clone();
                let cancel_for_stream = cancel_flag.clone();
                let cancel_for_orch = cancel_flag.clone();

                #[allow(clippy::type_complexity)]
                let post_tool_wave: Arc<
                    Mutex<Option<(WaveAnimation, Arc<AtomicBool>)>>,
                > = Arc::new(Mutex::new(None));
                let ptw_for_complete = post_tool_wave.clone();
                let ptw_for_orch = post_tool_wave.clone();

                let input_buf_for_complete = input_buf.clone();
                let input_buf_for_orch = input_buf.clone();

                // --- Orchestrator display state ---
                struct OrchTaskInfo {
                    header_line_num: u32,
                    tools: Vec<String>,
                    color: (u8, u8, u8),
                }
                struct OrchDisplayState {
                    tasks: std::collections::HashMap<String, OrchTaskInfo>,
                }
                let orch_state: Arc<Mutex<OrchDisplayState>> =
                    Arc::new(Mutex::new(OrchDisplayState {
                        tasks: std::collections::HashMap::new(),
                    }));
                let orch_for_cb = orch_state.clone();
                // Set after tool_call_completed; consumed before the next
                // non-tool event to insert a blank line separator.
                let needs_blank = Arc::new(AtomicBool::new(false));
                let needs_blank_for_cb = needs_blank.clone();

                let tool_defs = tools::client_tool_definitions();
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
                        header.with(crossterm::style::Color::White),
                    );

                    // File path
                    println!(
                        "{} {}",
                        "├─".with(crossterm::style::Color::DarkGrey),
                        ctx.file_path
                            .as_str()
                            .with(crossterm::style::Color::DarkGrey),
                    );

                    if ctx.shell_calls.is_empty() {
                        println!(
                            "{} {}",
                            "└─".with(crossterm::style::Color::DarkGrey),
                            "No changes made".with(crossterm::style::Color::DarkGrey),
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
                let in_update_for_tool = in_update_group.clone();

                // Tool execution loop: send request → process stream → if tool calls,
                // execute locally and send results back → repeat until text response.
                'tool_loop: loop {
                    let result = rt.block_on(async {
                        backend.stream_chat(
                            conversation.messages(),
                            Some(&tool_defs),
                            cancel_for_stream.clone(),
                            // on_token — no-op: text is accumulated silently (not
                            // displayed per-token), so the animation should keep
                            // running to provide visual feedback. It will be stopped
                            // by on_tool_complete, the ToolCalls handler, or
                            // post-loop cleanup. The animation thread handles stdin.
                            |_token| {},
                            // on_tool_requested — record args; don't stop animation
                            // (animation keeps running until something needs to display)
                            |tool_name, args| {
                                // Record args for later pairing with on_tool_complete
                                if let Ok(mut map) = pending_args_for_tool.lock() {
                                    map.insert(tool_name.to_string(), args.clone());
                                }
                                // Track Update grouping flag for the execution loop
                                if tool_name == "Update" {
                                    in_update_for_tool.store(true, Ordering::Relaxed);
                                } else if tool_name != "Shell"
                                    || !in_update_for_tool.load(Ordering::Relaxed)
                                {
                                    // Non-Shell (or Shell outside Update) clears the flag
                                    if tool_name != "Shell" {
                                        in_update_for_tool.store(false, Ordering::Relaxed);
                                    }
                                }
                            },
                            // on_tool_start — no-op; animation keeps running
                            |_tool_name| {},
                            // on_tool_complete — show grouped summary for server-side tools
                            |tool_name, duration, result: Option<&str>| {
                                // Stop animation: prefer ptw (later iterations), fall back
                                // to initial animation (first iteration).
                                let had_ptw = if let Ok(mut guard) = ptw_for_complete.lock() {
                                    if let Some((ptw_anim, _)) = guard.take() {
                                        ptw_anim.finish();
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                };
                                if !had_ptw && !anim_cleared_for_complete.load(Ordering::Relaxed) {
                                    stop_and_clear_animation(&stop_flag_for_token);
                                    anim_cleared_for_complete.store(true, Ordering::Relaxed);
                                }
                                // Show server-side tool result
                                {
                                    let _term = lock_term();
                                    erase_input_frame();
                                    let args = pending_args_for_complete
                                        .lock()
                                        .ok()
                                        .and_then(|mut map| map.remove(tool_name))
                                        .unwrap_or_default();
                                    if is_expanded_output() {
                                        print_tool_call_expanded(
                                            tool_name,
                                            &args,
                                            duration,
                                            result,
                                        );
                                    } else {
                                        crate::tools::print_tool_call_tree(tool_name, &args, 2);
                                        println!();
                                    }
                                    if let Ok(mut events) = turn_events_for_complete.lock() {
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
                                        input_buf_for_complete.clone(),
                                        Some(cancel_for_complete.clone()),
                                    )
                                };
                                if let Ok(mut guard) = ptw_for_complete.lock() {
                                    *guard = Some((wave_anim, wave_stop));
                                }
                                prepare_input_line(
                                    &input_buf_for_complete,
                                    Some(&cancel_for_complete),
                                );
                            },
                            // on_usage
                            |prompt_tokens, completion_tokens| {
                                set_status_bar_tokens(prompt_tokens, completion_tokens);
                                update_status_bar();
                                if let Ok(mut events) = turn_events_for_usage.lock() {
                                    events.push(DisplayEvent::Usage {
                                        prompt_tokens,
                                        completion_tokens,
                                    });
                                }
                            },
                            // on_raw_event — capture for stream panel
                            |event_name, event_data| {
                                push_sse_event(event_name, event_data);
                            },
                            // on_orchestrator_event — display orchestrator events in chat
                            |event_name, val| {
                                // Handle aura.progress — single-agent progress messages
                                if event_name == "aura.progress" {
                                    if let Some(message) = val.get("message").and_then(|v| v.as_str()) {
                                        crate::ui::prompt::set_agent_reasoning(message);
                                    }
                                    return;
                                }

                                let sub = event_name.strip_prefix("aura.orchestrator.").unwrap_or(event_name);
                                if event_name == "aura.session_info" || sub == event_name {
                                    return;
                                }


                                // Stop current animation
                                let had_ptw = if let Ok(mut guard) = ptw_for_orch.lock() {
                                    if let Some((ptw_anim, _)) = guard.take() {
                                        ptw_anim.finish();
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                };
                                if !had_ptw && !anim_cleared_for_orch.load(Ordering::Relaxed) {
                                    stop_and_clear_animation(&stop_flag_for_orch);
                                    anim_cleared_for_orch.store(true, Ordering::Relaxed);
                                }
                                {
                                let _term = lock_term();
                                erase_input_frame();

                                // Emit a deferred blank line from a previous
                                // tool_call_completed — but only if this event
                                // isn't another tool starting directly after.
                                if needs_blank_for_cb.swap(false, Ordering::Relaxed) && sub != "tool_call_started" {
                                    println!();
                                    crate::ui::prompt::increment_orch_scrollback();
                                }

                                // Helpers
                                let get_str = |v: &serde_json::Value, field: &str| -> String {
                                    let f = v.get(field)
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
                                let parse_args = |v: &serde_json::Value| -> Option<serde_json::Map<String, serde_json::Value>> {
                                    v.get("arguments")
                                        .or_else(|| v.get("data").and_then(|d| d.get("arguments")))
                                        .and_then(|a| match a {
                                            serde_json::Value::Object(obj) => Some(obj.clone()),
                                            serde_json::Value::String(s) => serde_json::from_str(s).ok(),
                                            _ => None,
                                        })
                                };

                                // Pick bullet color: look up from task state if this event
                                // has a task_id, otherwise use a fresh random color.
                                let event_task_id = get_str(val, "task_id");
                                let (cr, cg, cb) = if !event_task_id.is_empty() {
                                    orch_for_cb.lock().ok()
                                        .and_then(|os| os.tasks.get(&event_task_id).map(|t| t.color))
                                        .unwrap_or_else(|| {
                                            let c = random_bullet_color();
                                            if let crossterm::style::Color::Rgb { r, g, b } = c { (r, g, b) } else { (255, 255, 255) }
                                        })
                                } else {
                                    let c = random_bullet_color();
                                    if let crossterm::style::Color::Rgb { r, g, b } = c { (r, g, b) } else { (255, 255, 255) }
                                };
                                let bullet_color = crossterm::style::Color::Rgb { r: cr, g: cg, b: cb };
                                let grey = crossterm::style::Color::DarkGrey;

                                match sub {
                                    "plan_created" => {
                                        let goal = get_str(val, "goal");
                                        // New plan resets reasoning
                                        crate::ui::prompt::clear_agent_reasoning();
                                        println!(
                                            "{} {}",
                                            "●".with(bullet_color).attribute(crossterm::style::Attribute::Bold),
                                            format!("Plan - {goal}").attribute(crossterm::style::Attribute::Bold),
                                        );
                                        crate::ui::prompt::increment_orch_scrollback();
                                        if is_expanded_output() {
                                            // Count lines that print_fields_tree will emit
                                            let field_lines = fields.values().map(|v| {
                                                if let serde_json::Value::Object(obj) = v { 1 + obj.len() } else { 1 }
                                            }).sum::<usize>();
                                            print_fields_tree(&fields);
                                            for _ in 0..field_lines {
                                                crate::ui::prompt::increment_orch_scrollback();
                                            }
                                        }
                                        println!();
                                        crate::ui::prompt::increment_orch_scrollback();
                                        push_display_event(DisplayEvent::OrchestratorPlanCreated { goal, bullet_color: (cr, cg, cb), fields });
                                    }
                                    "task_started" => {
                                        let worker_id = get_str(val, "worker_id");
                                        let task_id = get_str(val, "task_id");
                                        let description = get_str(val, "description");
                                        // Assign a fresh random color for this task
                                        let task_color = random_bullet_color();
                                        let (tr, tg, tb) = if let crossterm::style::Color::Rgb { r, g, b } = task_color { (r, g, b) } else { (cr, cg, cb) };
                                        // Set reasoning in the Thinking animation sub-line
                                        crate::ui::prompt::set_agent_reasoning(&description);
                                        // Print task header with worker on same line
                                        // Record the scrollback line number BEFORE incrementing
                                        let header_line = crate::ui::prompt::current_orch_scrollback();
                                        println!(
                                            "{} {} {} {}",
                                            "●".with(task_color).attribute(crossterm::style::Attribute::Bold),
                                            format!("Task {task_id}").attribute(crossterm::style::Attribute::Bold),
                                            "-".with(grey),
                                            format!("Worker: {worker_id}").with(grey),
                                        );
                                        crate::ui::prompt::increment_orch_scrollback();
                                        // No blank line – tool calls follow directly
                                        // Track task in state with its assigned color
                                        if let Ok(mut os) = orch_for_cb.lock() {
                                            os.tasks.insert(task_id.clone(), OrchTaskInfo {
                                                header_line_num: header_line,
                                                tools: Vec::new(),
                                                color: (tr, tg, tb),
                                            });
                                        }
                                        push_display_event(DisplayEvent::OrchestratorTaskStarted {
                                            worker_id, task_id, description, bullet_color: (tr, tg, tb), fields,
                                        });
                                    }
                                    "tool_call_started" => {
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
                                                                if s.len() > 20 { format!("\"{}...\"", &s[..17]) } else { format!("\"{s}\"") }
                                                            }
                                                            other => { let s = other.to_string(); if s.len() > 20 { format!("{}...", &s[..17]) } else { s } }
                                                        };
                                                        format!("{k}: {val_str}")
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join(", ")
                                            })
                                            .unwrap_or_default();
                                        let reasoning = args_obj.as_ref()
                                            .and_then(|obj| obj.get("_aura_reasoning").and_then(|v| v.as_str()));
                                        // Set reasoning in the Thinking animation sub-line
                                        if let Some(text) = reasoning {
                                            crate::ui::prompt::set_agent_reasoning(text);
                                        }
                                        // Print tool line indented under the task with live duration
                                        let tool_display = format!("{display_name}({args_summary})");
                                        // Use tool_call_id as primary key, fall back to tool_initiator_id
                                        let match_id = if !tool_call_id.is_empty() { &tool_call_id } else { &tool_initiator_id };
                                        crate::ui::prompt::register_orch_tool(
                                            match_id,
                                            &task_id_str,
                                            &tool_display,
                                            std::time::Instant::now(),
                                            (cr, cg, cb),
                                            &fields,
                                        );
                                        // Track tool under its task
                                        if let Ok(mut os) = orch_for_cb.lock() {
                                            if let Some(task) = os.tasks.get_mut(&task_id_str) {
                                                task.tools.push(tool_display);
                                            } else if let Some(task) = os.tasks.get_mut(&tool_initiator_id) {
                                                task.tools.push(tool_display);
                                            }
                                        }
                                        push_display_event(DisplayEvent::OrchestratorToolCallStarted {
                                            tool_name, tool_initiator_id, bullet_color: (cr, cg, cb), fields,
                                        });
                                    }
                                    "tool_call_completed" => {
                                        let tool_name = get_str(val, "tool_name");
                                        let tool_initiator_id = get_str(val, "tool_initiator_id");
                                        let tool_call_id = get_str(val, "tool_call_id");
                                        let duration_ms_val = val.get("duration_ms")
                                            .or_else(|| val.get("data").and_then(|d| d.get("duration_ms")))
                                            .and_then(|v| v.as_u64());
                                        // Use tool_call_id as primary key, fall back to tool_initiator_id
                                        let match_id = if !tool_call_id.is_empty() { &tool_call_id } else { &tool_initiator_id };
                                        // Finalize tool display (solid color + completed duration)
                                        crate::ui::prompt::finalize_orch_tool(
                                            match_id,
                                            duration_ms_val,
                                            (cr, cg, cb),
                                        );
                                        needs_blank_for_cb.store(true, Ordering::Relaxed);
                                        push_display_event(DisplayEvent::OrchestratorToolCallCompleted {
                                            tool_name, tool_initiator_id, bullet_color: (cr, cg, cb),
                                            duration_ms: duration_ms_val, fields,
                                        });
                                    }
                                    "task_completed" => {
                                        let worker_id = get_str(val, "worker_id");
                                        let task_id = get_str(val, "task_id");
                                        let result = get_str(val, "result");
                                        // Look up task info and overwrite the header line in-place
                                        let task_info = if let Ok(mut os) = orch_for_cb.lock() {
                                            os.tasks.remove(&task_id)
                                        } else {
                                            None
                                        };
                                        if let Some(info) = &task_info {
                                            overwrite_orch_task_header_unlocked(
                                                info.header_line_num,
                                                &task_id,
                                                &worker_id,
                                                info.color,
                                            );
                                        }
                                        crate::ui::prompt::clear_orch_task_tools(&task_id);
                                        // Blank line to separate from next task
                                        println!();
                                        crate::ui::prompt::increment_orch_scrollback();
                                        push_display_event(DisplayEvent::OrchestratorTaskCompleted {
                                            worker_id, task_id, result, bullet_color: (cr, cg, cb), fields,
                                        });
                                    }
                                    "synthesizing" => {
                                        crate::ui::prompt::clear_agent_reasoning();
                                        push_display_event(DisplayEvent::OrchestratorSynthesizing { bullet_color: (cr, cg, cb) });
                                    }
                                    "iteration_complete" => {
                                        let iteration = get_u64(val, "iteration");
                                        let quality_score = get_str(val, "quality_score");
                                        let expanded = is_expanded_output();
                                        let has_fields = expanded && !fields.is_empty();
                                        crate::ui::prompt::clear_agent_reasoning();
                                        println!(
                                            "{} {}",
                                            "●".with(bullet_color).attribute(crossterm::style::Attribute::Bold),
                                            "Iteration complete".attribute(crossterm::style::Attribute::Bold),
                                        );
                                        crate::ui::prompt::increment_orch_scrollback();
                                        println!(
                                            "{} iteration: {}",
                                            "├─".with(grey),
                                            iteration.to_string().as_str().with(grey),
                                        );
                                        crate::ui::prompt::increment_orch_scrollback();
                                        let quality_connector = if has_fields { "├─" } else { "└─" };
                                        println!(
                                            "{} quality: {}",
                                            quality_connector.with(grey),
                                            quality_score.as_str().with(grey),
                                        );
                                        crate::ui::prompt::increment_orch_scrollback();
                                        if has_fields {
                                            let field_lines = fields.values().map(|v| {
                                                if let serde_json::Value::Object(obj) = v { 1 + obj.len() } else { 1 }
                                            }).sum::<usize>();
                                            print_fields_tree(&fields);
                                            for _ in 0..field_lines {
                                                crate::ui::prompt::increment_orch_scrollback();
                                            }
                                        }
                                        println!();
                                        crate::ui::prompt::increment_orch_scrollback();
                                        push_display_event(DisplayEvent::OrchestratorIterationComplete {
                                            iteration, quality_score, bullet_color: (cr, cg, cb), fields,
                                        });
                                    }
                                    "scratchpad_usage" => {
                                        let tokens_intercepted = get_u64(val, "tokens_intercepted");
                                        let tokens_extracted = get_u64(val, "tokens_extracted");
                                        crate::ui::prompt::add_scratchpad_usage(tokens_intercepted, tokens_extracted);
                                        update_status_bar_unlocked();
                                        push_display_event(DisplayEvent::OrchestratorScratchpadSavings {
                                            tokens_intercepted, tokens_extracted,
                                        });
                                    }
                                    _ => {}
                                }
                                } // drop _term lock

                                // Restart animation
                                let anim_label = if sub == "synthesizing" { "Synthesizing" } else { "Thinking" };
                                let (wave_anim, wave_stop) = {
                                    let _term = lock_term();
                                    WaveAnimation::start(
                                        anim_label,
                                        vec![],
                                        input_buf_for_orch.clone(),
                                        Some(cancel_for_orch.clone()),
                                    )
                                };
                                if let Ok(mut guard) = ptw_for_orch.lock() {
                                    *guard = Some((wave_anim, wave_stop));
                                }
                                prepare_input_line(
                                    &input_buf_for_orch,
                                    Some(&cancel_for_orch),
                                );
                            },
                        )
                        .await
                    });

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
                                        "└─".with(crossterm::style::Color::DarkGrey),
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
                                        "└─".with(crossterm::style::Color::DarkGrey),
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
                            let mut batch_tools: Vec<(String, String, String)> = Vec::new();
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
                                        "CompactContext()".with(crossterm::style::Color::White),
                                    );
                                    println!(
                                        "{} {}",
                                        "└─".with(crossterm::style::Color::DarkGrey),
                                        result_msg.as_str().with(crossterm::style::Color::DarkGrey),
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
                                        display.with(crossterm::style::Color::White),
                                    );

                                    // Check permissions for Update
                                    let perm = permissions.check(&tc.name, &tc.arguments);
                                    match perm {
                                        crate::permissions::PermissionResult::Denied(reason) => {
                                            in_update_group.store(false, Ordering::Relaxed);
                                            eprintln!(
                                                "  {}",
                                                reason
                                                    .as_str()
                                                    .with(crossterm::style::Color::Yellow)
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
                                    // Check deny list — explicit deny rules are always respected
                                    let shell_perm = permissions.check("Shell", &tc.arguments);
                                    if let crate::permissions::PermissionResult::Denied(reason) =
                                        shell_perm
                                    {
                                        eprintln!(
                                            "  {}",
                                            reason.as_str().with(crossterm::style::Color::Yellow)
                                        );
                                        let rules = permissions.describe_rules();
                                        let denied_msg = tools::permission_denied_message(
                                            "Shell",
                                            &reason,
                                            rules.as_deref(),
                                        );
                                        conversation.add_tool_result(&tc.id, &tc.name, &denied_msg);
                                        continue;
                                    }

                                    // Auto-approve: execute the Shell call
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
                                    let result =
                                        server_results.get(&tc.id).cloned().unwrap_or_else(|| {
                                            format!("Server tool {} executed successfully", tc.name)
                                        });
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
                                        display.with(crossterm::style::Color::White),
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
                                            reason.as_str().with(crossterm::style::Color::Yellow)
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
                                ));

                                // Add tool result to conversation history
                                conversation.add_tool_result(&tc.id, &tc.name, &tool_result);
                            }

                            // Print summaries for batch of local tools
                            if !batch_tools.is_empty() {
                                let mut groups: Vec<(String, Vec<String>, Option<String>)> =
                                    Vec::new();
                                for (name, display, args) in &batch_tools {
                                    if let Some(group) =
                                        groups.iter_mut().find(|(n, _, _)| n == name)
                                    {
                                        group.1.push(display.clone());
                                    } else {
                                        groups.push((
                                            name.clone(),
                                            vec![display.clone()],
                                            Some(args.clone()),
                                        ));
                                    }
                                }
                                for (name, displays, first_args) in &groups {
                                    if displays.len() == 1 {
                                        // Single call: show tool call as key/value tree
                                        let args_str = first_args.as_deref().unwrap_or("{}");
                                        let args_map: std::collections::BTreeMap<
                                            String,
                                            serde_json::Value,
                                        > = serde_json::from_str(args_str).unwrap_or_default();
                                        tools::print_tool_call_tree(name, &args_map, 2);
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
                        "Canceled request".attribute(crossterm::style::Attribute::Bold),
                    );
                    println!(
                        "{} {}",
                        "└─".with(crossterm::style::Color::DarkGrey),
                        "User requested.".with(crossterm::style::Color::DarkGrey),
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
                        "Error".with(crossterm::style::Color::Red),
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
                            "└─".with(crossterm::style::Color::DarkGrey),
                            hint.as_str().with(crossterm::style::Color::Yellow),
                        );
                        push_display_event(DisplayEvent::Error(hint.clone()));
                        conversation.add_assistant(&format!("[Error: {}]", hint));
                    } else {
                        eprintln!(
                            "{} {}",
                            "└─".with(crossterm::style::Color::DarkGrey),
                            format!("{:#}", e).with(crossterm::style::Color::Yellow),
                        );
                        push_display_event(DisplayEvent::Error(format!("{:#}", e)));
                        conversation.add_assistant(&format!("[Error: {}]", e));
                    }
                } else {
                    if !final_text.is_empty() {
                        // Start summarize thinking animation
                        let (summarize_anim, _) = WaveAnimation::start(
                            "Thinking",
                            vec![],
                            input_buf.clone(),
                            Some(cancel_flag.clone()),
                        );
                        prepare_input_line(&input_buf, Some(&cancel_flag));

                        let summarize_result = rt.block_on(backend.summarize(&final_text));

                        summarize_anim.finish();
                        erase_input_frame();

                        let (summary, usage) = if cancel_flag.load(Ordering::Relaxed) {
                            ("Response".to_string(), None)
                        } else {
                            summarize_result.unwrap_or(("Response".to_string(), None))
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
                            text: final_text.clone(),
                        });

                        println!(
                            "{} {}",
                            "●"
                                .with(random_bullet_color())
                                .attribute(crossterm::style::Attribute::Bold),
                            summary.attribute(crossterm::style::Attribute::Bold),
                        );

                        println!();

                        render_markdown(&final_text);
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
            "aura-cli --resume".with(crossterm::style::Color::Cyan),
            short_id.with(crossterm::style::Color::Cyan),
        );
    }

    println!();
    Ok(())
}
