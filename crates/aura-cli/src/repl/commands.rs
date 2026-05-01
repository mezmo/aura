use crossterm::style::Stylize;
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use std::io::{self, Write};
use std::sync::atomic::Ordering;

use crate::api::types::DisplayEvent;
use crate::repl::conversations::ConversationStore;
use crate::repl::history::ConversationHistory;
use crate::repl::input_reader::{AuraHelper, HISTORY_COUNT, HISTORY_DEPTH};
use crate::ui::prompt::{
    clear_display_events, clear_stream_events, clear_stream_panel_in_place, extend_display_events,
    get_model_matches, is_expanded_output, list_conversations, load_and_restore_sse_events,
    print_help, print_welcome_state, redraw_input_frame, replay_event_log_global,
    reset_status_bar_tokens, seed_model_cache, seed_status_bar_tokens, set_expanded_output,
    set_mid_stream_history, set_selected_model, set_stream_conv_dir, set_stream_show_all,
    set_welcome_state, toggle_stream_panel, with_event_log,
};
use crate::ui::state::{RESUME_MATCHES, get_tab_select_index, set_tab_select_index};
use crate::ui::welcome::WelcomeState;

/// Handle the `/clear` command: save/delete the current conversation, reset state,
/// and redisplay the welcome screen.
pub(crate) fn handle_clear(
    conversation: &mut ConversationHistory,
    conv_store: &mut Option<ConversationStore>,
    input_reader: &mut Editor<AuraHelper, DefaultHistory>,
) {
    // Save current conversation if it has content, or delete if never started
    if let Some(store) = conv_store {
        if conversation.messages().len() > 1 {
            with_event_log(|log| {
                store.save_all(conversation.messages(), log, is_expanded_output())
            });
        } else {
            store.delete();
        }
    }
    conversation.clear();
    clear_display_events();
    clear_stream_events();
    set_selected_model(None);
    crate::ui::prompt::reset_orch_tools();
    *conv_store = ConversationStore::new().ok();
    set_stream_conv_dir(conv_store.as_ref().map(|s| s.dir().to_path_buf()));
    // Reset history for the new conversation
    input_reader.clear_history().ok();
    HISTORY_COUNT.store(0, Ordering::Relaxed);
    HISTORY_DEPTH.store(0, Ordering::Relaxed);
    set_mid_stream_history(Vec::new());
    // Pick fresh welcome for the new conversation
    set_welcome_state(WelcomeState::pick());

    // Clear the screen and reprint the welcome
    let mut stdout = io::stdout();
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
    );
    reset_status_bar_tokens();
    print_welcome_state();

    redraw_input_frame();
}

/// Handle the `/help` command.
pub(crate) fn handle_help() {
    print_help();
    redraw_input_frame();
}

/// Handle the `/expand` command: toggle expanded output and replay the event log.
pub(crate) fn handle_expand(
    conversation: &ConversationHistory,
    conv_store: &Option<ConversationStore>,
) {
    let expanded = !is_expanded_output();
    set_expanded_output(expanded);
    set_stream_show_all(expanded);
    crate::ui::prompt::erase_input_frame();
    replay_event_log_global();
    if let Some(store) = conv_store {
        with_event_log(|log| store.save_all(conversation.messages(), log, expanded));
    }
    redraw_input_frame();
}

/// Handle the `/stream` command: toggle the stream panel.
///
/// Called after the main loop has already erased the input frame and
/// reset geometry, so we only need to clear old panel content, toggle,
/// and redraw.
pub(crate) fn handle_stream() {
    // Clear the panel area BEFORE toggling so we erase the old content
    // when hiding, or clear stale content when showing.
    clear_stream_panel_in_place();
    toggle_stream_panel();
    redraw_input_frame();
}

/// Handle the `/conversations` command.
pub(crate) fn handle_conversations() {
    list_conversations();
    redraw_input_frame();
}

/// Handle the `/rename <name>` command.
pub(crate) fn handle_rename(arg: &str, conv_store: &Option<ConversationStore>) {
    if arg.is_empty() {
        println!("Usage: /rename <name>");
    } else if let Some(store) = conv_store {
        store.set_name(arg);
        println!("Conversation renamed to: {}", arg);
    } else {
        println!("No active conversation to rename.");
    }
    redraw_input_frame();
}

/// Handle the `/resume <id or name>` command.
/// Returns the new initial_input if any was loaded from the resumed conversation.
pub(crate) fn handle_resume(
    arg: &str,
    conversation: &mut ConversationHistory,
    conv_store: &mut Option<ConversationStore>,
    input_reader: &mut Editor<AuraHelper, DefaultHistory>,
    system_prompt: Option<&str>,
) -> Option<String> {
    // Check if a conversation was selected via Tab (before empty check)
    if let Some(tab_idx) = get_tab_select_index() {
        let resume_matches = RESUME_MATCHES.lock().map(|g| g.clone()).unwrap_or_default();
        if let Some((uuid, _)) = resume_matches.get(tab_idx) {
            let tab_arg = uuid.clone();
            set_tab_select_index(None);
            return handle_resume(
                &tab_arg,
                conversation,
                conv_store,
                input_reader,
                system_prompt,
            );
        }
        set_tab_select_index(None);
    }
    if arg.is_empty() {
        println!("Usage: /resume <id or name>");
        println!("Use /conversations to list available conversations.");
        redraw_input_frame();
        return None;
    }
    // Use find_matching to resolve by UUID prefix or name substring
    let matches = ConversationStore::find_matching(arg);
    let full_uuid = if matches.len() == 1 {
        matches[0].0.clone()
    } else if matches.is_empty() {
        println!("No conversation found matching '{}'.", arg);
        println!("Use /conversations to list available conversations.");
        redraw_input_frame();
        return None;
    } else {
        // Ambiguous — shouldn't happen if Enter gating works, but handle gracefully
        println!("Ambiguous match '{}'. Matching conversations:", arg);
        for (uuid, name) in &matches {
            let short = &uuid[..8.min(uuid.len())];
            let display_name = if name.is_empty() {
                "(untitled)"
            } else {
                name.trim()
            };
            println!("  {} {}", short, display_name);
        }
        redraw_input_frame();
        return None;
    };
    // Save current conversation before switching, or delete if never started
    if let Some(store) = conv_store {
        if conversation.messages().len() > 1 {
            with_event_log(|log| {
                store.save_all(conversation.messages(), log, is_expanded_output())
            });
        } else {
            store.delete();
        }
    }
    let mut new_initial_input = None;
    match resume_conversation(&full_uuid, system_prompt) {
        Some((store, history, events, was_expanded, _usage_totals)) => {
            *conv_store = Some(store);
            set_stream_conv_dir(conv_store.as_ref().map(|s| s.dir().to_path_buf()));
            *conversation = history;
            clear_display_events();
            extend_display_events(events);
            set_expanded_output(was_expanded);
            // Restore SSE stream panel events
            if let Some(s) = conv_store {
                load_and_restore_sse_events(s.dir());
            }
            // Restore selected model and model cache
            if let Some(s) = conv_store {
                if let Some(model) = s.load_model() {
                    set_selected_model(Some(model));
                } else {
                    set_selected_model(None);
                }
                if let Some(models) = s.load_models_cache() {
                    seed_model_cache(models);
                }
            }
            // Load per-conversation input history for the resumed conversation
            input_reader.clear_history().ok();
            if let Some(s) = conv_store {
                let entries = s.load_input_history();
                HISTORY_COUNT.store(entries.len(), Ordering::Relaxed);
                for entry in &entries {
                    let _ = input_reader.add_history_entry(entry);
                }
                set_mid_stream_history(entries);
                if let Some(pending) = s.load_pending_input() {
                    new_initial_input = Some(pending);
                }
            }
            HISTORY_DEPTH.store(0, Ordering::Relaxed);
            // Pick fresh welcome for the resumed conversation
            set_welcome_state(WelcomeState::pick());
            // Replay the event log so the user sees the conversation
            crate::ui::prompt::erase_input_frame();
            replay_event_log_global();
            // Seed token counters from authoritative usage JSONL after replay
            if let Some(store) = conv_store {
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
        None => {
            redraw_input_frame();
        }
    }
    new_initial_input
}

/// Handle the `/model` or `/model <filter>` command.
pub(crate) fn handle_model(filter: &str, conv_store: &Option<ConversationStore>) {
    // Check if a model was selected via Tab
    if let Some(tab_idx) = get_tab_select_index() {
        let matches = get_model_matches();
        if let Some(model_id) = matches.get(tab_idx) {
            let model_id = model_id.clone();
            set_selected_model(Some(model_id.clone()));
            if let Some(store) = conv_store {
                store.save_model(&model_id);
            }
            println!("Model set to: {}", model_id);
            set_tab_select_index(None);
            redraw_input_frame();
            return;
        }
        set_tab_select_index(None);
    }
    let matches = get_model_matches();
    if matches.len() == 1 {
        // Exact or unique match — use it directly
        let model_id = matches[0].clone();
        set_selected_model(Some(model_id.clone()));
        if let Some(store) = conv_store {
            store.save_model(&model_id);
        }
        println!("Model set to: {}", model_id);
    } else if !filter.is_empty() {
        // Check if filter exactly matches a listed model (case-insensitive)
        let exact = matches.iter().find(|m| m.eq_ignore_ascii_case(filter));
        if let Some(model_id) = exact {
            let model_id = model_id.clone();
            set_selected_model(Some(model_id.clone()));
            if let Some(store) = conv_store {
                store.save_model(&model_id);
            }
            println!("Model set to: {}", model_id);
        } else {
            // Unlisted model — ask for confirmation with immediate keypress
            use crossterm::event::{self, Event, KeyCode as CKC, KeyEventKind};
            print!(
                "\"{}\" is not in the server's model list. Use it anyway? (y/n) ",
                filter,
            );
            let _ = io::stdout().flush();
            crossterm::terminal::enable_raw_mode().ok();
            let accepted = loop {
                if let Ok(Event::Key(key)) = event::read() {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        CKC::Char('y') | CKC::Char('Y') => break true,
                        _ => break false,
                    }
                }
            };
            crossterm::terminal::disable_raw_mode().ok();
            println!();
            if accepted {
                let model_id = filter.to_string();
                set_selected_model(Some(model_id.clone()));
                if let Some(store) = conv_store {
                    store.save_model(&model_id);
                }
                println!("Model set to: {}", model_id);
            } else {
                println!("Model selection cancelled.");
            }
        }
    }
    redraw_input_frame();
}

/// Returned tuple: (store, history, events, expanded, (prompt_tokens, completion_tokens))
#[allow(clippy::type_complexity)]
pub(crate) fn resume_conversation(
    id_prefix: &str,
    system_prompt: Option<&str>,
) -> Option<(
    ConversationStore,
    ConversationHistory,
    Vec<DisplayEvent>,
    bool,
    (u64, u64),
)> {
    match ConversationStore::find_by_prefix(id_prefix) {
        Ok(full_uuid) => {
            let store = match ConversationStore::open(&full_uuid) {
                Ok(s) => s,
                Err(e) => {
                    println!("Error opening conversation: {}", e);
                    return None;
                }
            };
            let messages = store.load_chat_history().unwrap_or_default();
            let events = store.load_view().unwrap_or_default();
            let was_expanded = store.load_view_expanded();
            let usage_totals = store.load_usage_totals();

            if messages.is_empty() {
                println!("Conversation {} has no history.", &full_uuid[..8]);
                return None;
            }

            // If the loaded messages don't start with a system prompt but we have one,
            // prepend it. If they already have one, use as-is.
            let messages = if messages.first().map(|m| m.role.as_str()) != Some("system") {
                if let Some(prompt) = system_prompt {
                    let mut new_msgs = vec![crate::api::types::Message::system(prompt)];
                    new_msgs.extend(messages);
                    new_msgs
                } else {
                    messages
                }
            } else {
                messages
            };

            let history = ConversationHistory::from_messages(messages);
            Some((store, history, events, was_expanded, usage_totals))
        }
        Err(matches) if matches.is_empty() => {
            println!("No conversation found matching '{}'.", id_prefix);
            println!("Use /conversations to list available conversations.");
            None
        }
        Err(matches) => {
            println!("Ambiguous ID '{}'. Matching conversations:", id_prefix);
            for uuid in &matches {
                println!("  {}", &uuid[..8.min(uuid.len())]);
            }
            None
        }
    }
}
