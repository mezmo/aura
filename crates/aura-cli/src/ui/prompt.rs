// ---------------------------------------------------------------------------
// prompt.rs — Thin re-export layer
//
// All functionality has been extracted into focused sub-modules under `src/ui/`.
// This file re-exports everything that was previously `pub` so that existing
// callers (`crate::ui::prompt::foo`) continue to compile unchanged.
// ---------------------------------------------------------------------------

// Re-export from state.rs
pub use super::state::{
    ACTIVE_ORCH_TOOLS, ORCH_SCROLLBACK_COUNTER, cache_anim_lines, capture_style_preview_original,
    check_resize, clear_display_events, clear_queued_input, clear_style_preview_original,
    extend_display_events, frame_lines, get_model_cache, get_model_matches, get_selected_model,
    install_sigint_handler, is_expanded_output, is_pretty, is_processing, is_readline_active,
    last_mid_stream_history_entry, print_welcome_state, print_welcome_state_animated,
    push_display_event, push_mid_stream_history, random_bullet_color, reset_input_geometry,
    reset_task_colors, restore_style_preview_original, set_expanded_output, set_mid_stream_history,
    set_pretty, set_processing, set_queued_input, set_readline_active, set_selected_model,
    set_startup_status, set_welcome_state, take_queued_input, task_color_for, term_size,
    text_lines, with_event_log, with_event_log_mut,
};
pub(crate) use super::state::{lock_term, take_pending_command};

// Re-export from status_bar.rs
pub use super::status_bar::{
    add_scratchpad_usage, add_turn_notice, clear_turn_notices, get_cumulative_tokens, handle_ctrlc,
    reset_ctrlc_state, reset_status_bar_tokens, seed_status_bar_tokens, set_auto_compact_ceiling,
    set_status_bar, set_status_bar_tokens, update_status_bar,
};
pub(crate) use super::status_bar::{rebuild_status_bar, update_status_bar_unlocked};

// Re-export from input_frame.rs
pub use super::input_frame::{
    cleanup_terminal, clear_screen_preserve_frame, commit_cursor_row, erase_input_frame,
    handle_resize_frame, redraw_input_frame, restore_terminal_mode, set_noncanonical_noecho,
    setup_terminal, update_input_geometry,
};

// Re-export from animation.rs
pub use super::animation::{
    ToolStatusAnimation, WaveAnimation, finish_tool_call_line, print_tool_call_line,
    stop_and_clear_animation, tick_queued_wave,
};

// Re-export from stream_panel.rs
pub(crate) use super::stream_panel::clear_stream_panel_in_place;
pub use super::stream_panel::{
    RawSseEvent, at_stream_top, clear_stream_events, enter_stream_focus, exit_stream_focus,
    is_stream_panel_focused, is_stream_panel_visible, load_and_restore_sse_events, push_sse_event,
    scroll_stream_down, scroll_stream_page_down, scroll_stream_page_up, scroll_stream_up,
    set_stream_conv_dir, set_stream_show_all, toggle_stream_expand, toggle_stream_panel,
};

// Re-export from event_replay.rs
pub use super::event_replay::{
    list_conversations, print_fields_tree, print_help, print_tool_call_expanded,
    print_tool_call_summary, print_user_echo, replay_event_log_global,
};

// Re-export from orchestrator.rs
pub(crate) use super::orchestrator::overwrite_orch_task_header_unlocked;
pub use super::orchestrator::{
    ActiveOrchTool, OrchLastToolInfo, clear_agent_reasoning, clear_orch_task_tools,
    current_orch_scrollback, finalize_orch_tool, increment_orch_scrollback,
    increment_orch_scrollback_wrapped, overwrite_orch_task_header, record_tool_progress_token,
    register_orch_tool, reset_orch_tools, set_agent_reasoning, set_orch_tool_progress_by_token,
};

// Re-export from mid_stream.rs
pub use super::mid_stream::{drain_stdin, poll_input, prepare_input_line, styled_prompt};

// Re-export from input_hint.rs
pub use super::input_hint::{
    clear_input_hint, refresh_model_cache, seed_model_cache, set_model_cache, set_model_error,
    set_model_fetch_config, trigger_model_fetch, update_input_hint, validate_command_input,
};
