use crate::theme::{AuraStyle, Themed};
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
    crate::ui::prompt::reset_task_colors();
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

/// Handle the `/telemetry` command: `status` prints whether telemetry is
/// active and the disable reason if not; `recent [N]` prints the last
/// `N` records from the local inspection log (default 20). The user-
/// facing contract these commands satisfy is documented in
/// `docs/telemetry.md`.
///
/// Formatting lives in [`format_telemetry_status`] and
/// [`format_telemetry_recent`] so unit tests can lock the output
/// shape — terminal side-effects (`println!`, redraw) are confined to
/// this entry point.
pub(crate) fn handle_telemetry(arg: &str, telemetry: &aura_telemetry::TelemetryHandle) {
    let mut parts = arg.split_whitespace();
    let sub = parts.next().unwrap_or("status");
    let body = match sub {
        "status" => format_telemetry_status(telemetry),
        "recent" => {
            let n = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(20);
            format_telemetry_recent(telemetry, n)
        }
        "disable" => format_telemetry_disable_result(
            crate::config::save_telemetry_enabled_to_global_cli_toml(false),
        ),
        other => format!(
            "Unknown /telemetry subcommand: {other}\nAvailable: status, recent [N], disable"
        ),
    };
    println!("{body}");
    redraw_input_frame();
}

pub(crate) fn format_telemetry_disable_result(result: anyhow::Result<()>) -> String {
    match result {
        Ok(()) => "telemetry: persisted [telemetry] enabled = false in ~/.aura/cli.toml. \
                   The change takes effect on the next launch. Re-enable by removing \
                   the line, or set `enabled = true`."
            .to_string(),
        Err(e) => format!("could not persist /telemetry disable: {e}"),
    }
}

pub(crate) fn format_telemetry_status(telemetry: &aura_telemetry::TelemetryHandle) -> String {
    use aura_telemetry::inspection_log::disable_reason_label;
    let state = match telemetry.disable_reason() {
        None => "active".to_string(),
        Some(r) => format!("disabled ({})", disable_reason_label(r)),
    };
    let mut out = String::new();
    out.push_str(&format!("telemetry: {state}\n"));
    out.push_str(&format!("endpoint: {}\n", telemetry.endpoint()));
    out.push_str(&format!(
        "install-id path: {}\n",
        telemetry
            .install_id_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unset)".to_string())
    ));
    out.push_str(&format!(
        "inspection log: {}\n",
        telemetry
            .inspection_log_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(disabled — AURA_TELEMETRY_LOG_EVENTS=0)".to_string())
    ));
    out.push_str(&format!("session id: {}\n", telemetry.session_id()));
    out.push_str(&format!(
        "dropped (channel-full): {}\n",
        telemetry.dropped_count()
    ));
    out.push_str("see docs/telemetry.md for kill switches and the full event table.");
    out
}

pub(crate) fn format_telemetry_recent(
    telemetry: &aura_telemetry::TelemetryHandle,
    n: usize,
) -> String {
    let Some(log) = telemetry.inspection_log() else {
        return "inspection log is disabled (AURA_TELEMETRY_LOG_EVENTS=0).".to_string();
    };
    match log.recent(n) {
        Ok(events) => {
            if events.is_empty() {
                return "no telemetry events recorded yet.".to_string();
            }
            let mut out = format!("last {} event(s):", events.len());
            for evt in events {
                let suffix = if evt.sent {
                    "[sent]".to_string()
                } else {
                    match evt.not_sent_reason {
                        Some(r) => format!("[not sent — {r}]"),
                        None => "[not sent]".to_string(),
                    }
                };
                out.push_str(&format!(
                    "\n  {}  {}  {}",
                    evt.ts.format("%Y-%m-%dT%H:%M:%SZ"),
                    evt.event,
                    suffix
                ));
            }
            out
        }
        Err(e) => format!("could not read inspection log: {e}"),
    }
}

#[cfg(test)]
mod telemetry_command_tests {
    use super::*;
    use aura_telemetry::events::ServerStarted;
    use aura_telemetry::properties::{DeploymentMethod, OsFamily, Source};
    use aura_telemetry::{DisableReason, TelemetryConfig};
    use std::time::Duration;
    use tempfile::TempDir;
    use uuid::Uuid;

    struct TestHandle {
        handle: aura_telemetry::TelemetryHandle,
        _dir: TempDir,
    }

    fn build(disable: Option<DisableReason>) -> TestHandle {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("events.jsonl");
        let install_path = dir.path().join("install-id");
        let cfg = TelemetryConfig {
            endpoint: "http://127.0.0.1:1/no-such-host".into(),
            api_key: "phc_test".into(),
            install_id: Uuid::new_v4(),
            install_id_path: Some(install_path),
            session_id: Uuid::new_v4(),
            source: Source::Cli,
            os_family: OsFamily::current(),
            deployment_method: DeploymentMethod::Local,
            aura_version: "9.9.9-test",
            inspection_log_path: Some(log_path),
            disable_reason: disable,
            channel_capacity: 16,
            batch_size: 8,
            flush_interval: Duration::from_secs(60),
            http_client: None,
        };
        TestHandle {
            handle: aura_telemetry::init(cfg),
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn status_active_mentions_session_and_docs_link() {
        let h = build(None);
        let out = format_telemetry_status(&h.handle);
        assert!(out.starts_with("telemetry: active\n"), "got: {out}");
        assert!(out.contains("dropped (channel-full): 0"));
        assert!(out.contains("session id: "));
        assert!(out.contains("docs/telemetry.md"));
    }

    #[tokio::test]
    async fn status_includes_endpoint_install_id_path_and_log_path() {
        let h = build(None);
        let out = format_telemetry_status(&h.handle);
        assert!(out.contains("endpoint: http://127.0.0.1:1/no-such-host"), "got: {out}");
        assert!(
            out.contains("install-id path: ") && out.contains("install-id"),
            "expected install-id path line, got: {out}"
        );
        assert!(
            out.contains("inspection log: ") && out.contains("events.jsonl"),
            "expected inspection log line, got: {out}"
        );
    }

    #[tokio::test]
    async fn status_disabled_names_kill_switch() {
        let h = build(Some(DisableReason::DoNotTrack));
        let out = format_telemetry_status(&h.handle);
        assert!(out.contains("telemetry: disabled (DoNotTrack)"));
    }

    #[tokio::test]
    async fn recent_empty_message() {
        let h = build(None);
        let out = format_telemetry_recent(&h.handle, 10);
        assert_eq!(out, "no telemetry events recorded yet.");
    }

    #[tokio::test]
    async fn recent_lists_captured_events_with_sent_marker() {
        // The active path now writes the inspection-log row from the
        // background task after the POST result is known, so calling
        // `capture` then immediately reading would race. The
        // formatter itself is what we want to lock in here — given an
        // InspectedEvent with `sent: true`, it renders `[sent]`. We
        // assert that directly by appending the synthetic row.
        let h = build(None);
        let log = h
            .handle
            .inspection_log()
            .expect("inspection log present in fixture");
        log.append(&aura_telemetry::inspection_log::InspectedEvent {
            ts: chrono::Utc::now(),
            event: "server_started".into(),
            properties: serde_json::json!({"aura_source": "cli"}),
            sent: true,
            not_sent_reason: None,
        })
        .unwrap();
        let out = format_telemetry_recent(&h.handle, 5);
        assert!(out.starts_with("last 1 event(s):"), "got: {out}");
        assert!(out.contains("server_started"));
        assert!(out.contains("[sent]"));
    }

    #[tokio::test]
    async fn recent_disabled_run_marks_not_sent_with_reason() {
        let h = build(Some(DisableReason::DoNotTrack));
        h.handle.capture(ServerStarted {
            default_agent_set: false,
        });
        let out = format_telemetry_recent(&h.handle, 5);
        // First line is the synthetic telemetry_opt_out, second is the
        // captured event. Both must show the kill-switch reason.
        assert!(out.contains("telemetry_opt_out"));
        assert!(out.contains("server_started"));
        assert!(out.contains("[not sent — DoNotTrack]"));
    }

    #[test]
    fn disable_success_message_explains_next_step() {
        let out = format_telemetry_disable_result(Ok(()));
        assert!(out.contains("[telemetry] enabled = false"));
        assert!(out.contains("~/.aura/cli.toml"));
        assert!(out.contains("next launch"));
    }

    #[test]
    fn disable_failure_message_surfaces_error() {
        let out = format_telemetry_disable_result(Err(anyhow::anyhow!("disk full")));
        assert!(out.contains("could not persist"));
        assert!(out.contains("disk full"));
    }
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

/// Repaint everything after a theme switch — erases the input frame,
/// replays every recorded `DisplayEvent` under the new theme, redraws the
/// frame, and signals rustyline to do a full refresh on its next event so
/// the input cursor lands at the right column.
///
/// Call this after `set_theme(...)` (or after `restore_style_preview_original`)
/// to make the change visible.
pub(crate) fn repaint_after_style_change() {
    crate::ui::prompt::erase_input_frame();
    replay_event_log_global();
    redraw_input_frame();
    crate::ui::state::FORCE_REPAINT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Apply a style by name and live-repaint the visible scrollback. Used by
/// `handle_style` (Enter path) and the Tab preview handler. Returns `true`
/// if the name resolved to a known theme and was applied.
pub(crate) fn apply_style_live(name: &str) -> bool {
    let Some(t) = crate::theme::theme_by_name(name) else {
        return false;
    };
    crate::theme::set_theme(t);
    repaint_after_style_change();
    true
}

/// Persist the active theme to `~/.aura/cli.toml` after a `/style` commit.
/// On failure (no home dir, read-only fs, parse error, …), prints a
/// warning to stderr — the warning is intentionally NOT pushed to the
/// `EVENT_LOG` so it doesn't end up in saved chat transcripts.
fn save_active_style() {
    let public_name = crate::theme::theme_public_name(crate::theme::theme());
    if let Err(e) = crate::config::save_style_to_global_cli_toml(public_name) {
        eprintln!(
            "{}",
            format!("warning: could not persist style to ~/.aura/cli.toml: {e}")
                .themed(AuraStyle::Warning)
        );
    }
}

/// Handle the `/style [name]` command.
///
/// With no argument: prints the current style and the available options.
/// With an argument: switches to the named style. Tab-completion populates
/// `STYLE_MATCHES`; if the user pressed Tab and then Enter, the selected
/// match wins over the literal arg text.
pub(crate) fn handle_style(arg: &str) {
    use crate::theme::{STYLE_NAMES, theme};
    use crate::ui::state::STYLE_MATCHES;

    let arg = arg.trim();

    // Tab-selected name takes precedence over the typed arg.
    let tab_pick = get_tab_select_index()
        .and_then(|i| STYLE_MATCHES.lock().ok().and_then(|g| g.get(i).cloned()));
    set_tab_select_index(None);

    let chosen: Option<String> = tab_pick.or_else(|| {
        if arg.is_empty() {
            None
        } else {
            // Accept a unique prefix (so "/style high" works).
            let lower = arg.to_ascii_lowercase();
            let matches: Vec<&&str> = STYLE_NAMES
                .iter()
                .filter(|n| n.starts_with(&lower))
                .collect();
            if matches.len() == 1 {
                Some((*matches[0]).to_string())
            } else {
                Some(arg.to_string())
            }
        }
    });

    match chosen {
        None => {
            let current = theme().name;
            println!(
                "{}",
                format!("Current style: {current}").themed(AuraStyle::Muted)
            );
            println!(
                "{}",
                format!("Available: {}", STYLE_NAMES.join(", ")).themed(AuraStyle::Muted),
            );
            println!("{}", "Usage: /style <name>".themed(AuraStyle::Muted));
            redraw_input_frame();
        }
        Some(name) => {
            // Tab-preview may have already applied this theme; commit by
            // dropping the captured "revert target" so Esc no longer reverts.
            crate::ui::prompt::clear_style_preview_original();
            if apply_style_live(&name) {
                save_active_style();
            } else {
                println!(
                    "{}",
                    format!("Unknown style: {name}").themed(AuraStyle::Muted),
                );
                println!(
                    "{}",
                    format!("Available: {}", STYLE_NAMES.join(", ")).themed(AuraStyle::Muted),
                );
                redraw_input_frame();
            }
        }
    }
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
                "Resumed conversation. Continue below.".themed(AuraStyle::Success),
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
