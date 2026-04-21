use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use crossterm::style::Stylize;
use tokio::runtime::Runtime;

use crate::api::stream::StreamResult;
use crate::api::types::{ShellCallDetail, ToolCallInfo};
use crate::backend::Backend;
use crate::config::AppConfig;
use crate::repl::history::ConversationHistory;
use crate::tools;
use crate::ui::markdown::render_markdown;
use crate::ui::prompt::{random_bullet_color, set_selected_model};

pub fn run_oneshot(
    config: AppConfig,
    mut permissions: crate::permissions::PermissionChecker,
    backend: &Backend,
) -> Result<()> {
    let query = config
        .query
        .clone()
        .expect("query must be set for oneshot mode");

    // Initialize selected model from config (so config/CLI --model is respected)
    set_selected_model(config.model.clone());

    let rt = Runtime::new()?;
    let mut conversation = ConversationHistory::new(config.system_prompt.as_deref());

    conversation.add_user(&query);

    // Build tool defs only when client tools are enabled. When disabled the
    // CLI sends no `tools` field; the model can't request local execution
    // and the tool-call branches below stay dormant.
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
    let mut final_text = String::new();

    // Update grouping state (persists across loop iterations)
    struct OneshotUpdateContext {
        file_path: String,
        snapshot: Option<String>,
        shell_calls: Vec<ShellCallDetail>,
        commands_used: Vec<String>,
    }

    fn finalize_oneshot_update(ctx: OneshotUpdateContext) {
        let new_content = std::fs::read_to_string(&ctx.file_path).unwrap_or_default();
        let old_content = ctx.snapshot.as_deref().unwrap_or("");
        let (diff_text, lines_added, lines_removed) =
            tools::compute_diff(old_content, &new_content);
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
    }

    let mut active_update: Option<OneshotUpdateContext> = None;
    let in_update_group = Arc::new(AtomicBool::new(false));
    let in_update_for_tool = in_update_group.clone();

    // One-shot has no ConversationStore — generate a single process-lifetime
    // UUID so all turns within this invocation share a chat session.
    let session_uuid = uuid::Uuid::new_v4().to_string();
    let chat_session_id = crate::api::session::resolve_chat_session_id(
        &config.extra_headers,
        &session_uuid,
        crate::api::session::SessionKind::Chat,
    );

    // Tool execution loop
    let result: Result<()> = loop {
        let stream_result = rt.block_on(async {
            backend
                .stream_chat(
                    conversation.messages(),
                    tool_defs_arg,
                    &chat_session_id,
                    Arc::new(AtomicBool::new(false)),
                    // on_token — accumulate only; render after stream completes
                    |_token| {},
                    // on_tool_requested
                    |_tool_id, tool_name, _args| {
                        // Track Update grouping flag
                        if tool_name == "Update" {
                            in_update_for_tool.store(true, Ordering::Relaxed);
                        } else if tool_name != "Shell" {
                            in_update_for_tool.store(false, Ordering::Relaxed);
                        }
                        // Suppress display for all local tools — grouped summary
                        // will be shown after execution.
                    },
                    // on_tool_start — no display for server-side tools
                    |_tool_id, _tool_name| {},
                    // on_tool_complete — show server-side tool result
                    |_tool_id, tool_name, _duration: Duration, _result: Option<&str>| {
                        println!(
                            "{} {}",
                            "●"
                                .with(random_bullet_color())
                                .attribute(crossterm::style::Attribute::Bold),
                            tool_name.with(crossterm::style::Color::White),
                        );
                        println!();
                    },
                    // on_usage — no-op in oneshot mode
                    |_prompt_tokens, _completion_tokens| {},
                    // on_raw_event — no-op in oneshot mode
                    |_event_name, _event_data| {},
                    // on_orchestrator_event — no-op in oneshot mode
                    |_event_name, _val| {},
                )
                .await
        });

        match stream_result {
            Ok(StreamResult::TextResponse(text)) => {
                final_text = text;
                break Ok(());
            }
            Ok(StreamResult::ToolCalls {
                text,
                tool_calls,
                server_results,
            }) => {
                // Convert to ToolCallInfo for history
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

                // Add assistant message with tool calls
                let text_content = if text.is_empty() { None } else { Some(text) };
                conversation.add_assistant_with_tool_calls(text_content, tool_call_infos);

                // Execute each tool call, collecting info for grouped display
                let mut batch_tools: Vec<(String, String, String, std::time::Duration)> =
                    Vec::new();
                for tc in &tool_calls {
                    // --- Update tool grouping ---
                    if tc.name == "Update" {
                        // Finalize any previous active Update
                        if let Some(prev) = active_update.take() {
                            finalize_oneshot_update(prev);
                        }

                        let args: serde_json::Value =
                            serde_json::from_str(&tc.arguments).unwrap_or_default();
                        let file_path = args["file_path"].as_str().unwrap_or("?").to_string();

                        // Show what we're about to do before asking permission
                        let display = tools::format_tool_call_display(&tc.name, &tc.arguments);
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
                                    reason.as_str().with(crossterm::style::Color::Yellow)
                                );
                                let rules = permissions.describe_rules();
                                let denied_msg = tools::permission_denied_message(
                                    &tc.name,
                                    &reason,
                                    rules.as_deref(),
                                );
                                conversation.add_tool_result(&tc.id, &tc.name, &denied_msg);
                                continue;
                            }
                            crate::permissions::PermissionResult::Prompt => {
                                if !permissions.prompt_tool_permission(&tc.name, &tc.arguments) {
                                    in_update_group.store(false, Ordering::Relaxed);
                                    let reason = "denied by user".to_string();
                                    let rules = permissions.describe_rules();
                                    let denied_msg = tools::permission_denied_message(
                                        &tc.name,
                                        &reason,
                                        rules.as_deref(),
                                    );
                                    conversation.add_tool_result(&tc.id, &tc.name, &denied_msg);
                                    continue;
                                }
                            }
                            crate::permissions::PermissionResult::Allowed => {}
                        }

                        // Snapshot the file
                        let snapshot = std::fs::read_to_string(&file_path).ok();

                        active_update = Some(OneshotUpdateContext {
                            file_path: file_path.clone(),
                            snapshot,
                            shell_calls: Vec::new(),
                            commands_used: Vec::new(),
                        });

                        let result_msg = format!(
                            "Update context started for {file_path}. Use Shell calls to make changes."
                        );
                        conversation.add_tool_result(&tc.id, &tc.name, &result_msg);
                        continue;
                    }

                    // --- Shell within an active Update group ---
                    if tc.name == "Shell" && active_update.is_some() {
                        // Run the full permission check — Update approval does
                        // not implicitly trust arbitrary Shell commands. The
                        // user must allow each command (or pattern) explicitly.
                        let shell_perm = permissions.check("Shell", &tc.arguments);
                        match shell_perm {
                            crate::permissions::PermissionResult::Denied(reason) => {
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
                            crate::permissions::PermissionResult::Prompt => {
                                // Show the Shell call so the user has context
                                // for what they're approving — display is
                                // normally suppressed inside Update groups.
                                let display =
                                    tools::format_tool_call_display(&tc.name, &tc.arguments);
                                println!(
                                    "{} {}",
                                    "●"
                                        .with(random_bullet_color())
                                        .attribute(crossterm::style::Attribute::Bold),
                                    display.with(crossterm::style::Color::White),
                                );
                                if !permissions.prompt_tool_permission(&tc.name, &tc.arguments) {
                                    let reason = "denied by user".to_string();
                                    let rules = permissions.describe_rules();
                                    let denied_msg = tools::permission_denied_message(
                                        "Shell",
                                        &reason,
                                        rules.as_deref(),
                                    );
                                    conversation.add_tool_result(&tc.id, &tc.name, &denied_msg);
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
                        let full_cmd = args_val["command"].as_str().unwrap_or("").to_string();

                        if let Some(ref mut ctx) = active_update {
                            if !cmd_name.is_empty() && !ctx.commands_used.contains(&cmd_name) {
                                ctx.commands_used.push(cmd_name.clone());
                            }
                            ctx.shell_calls.push(ShellCallDetail {
                                command_name: cmd_name,
                                full_command: full_cmd,
                                result: tool_result.clone(),
                                duration,
                            });
                        }

                        conversation.add_tool_result(&tc.id, &tc.name, &tool_result);
                        continue;
                    }

                    // --- Non-Update, non-grouped tools ---

                    // If there's an active Update and we hit a non-Shell tool,
                    // finalize the Update first.
                    if let Some(prev) = active_update.take() {
                        finalize_oneshot_update(prev);
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
                                    "└─".with(crossterm::style::Color::Yellow),
                                    format!(
                                        "no result for server tool '{}' — server did not stream output (set AURA_CUSTOM_EVENTS=true)",
                                        tc.name
                                    )
                                    .with(crossterm::style::Color::Yellow),
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
                        let display = tools::format_tool_call_display(&tc.name, &tc.arguments);
                        println!(
                            "{} {}",
                            "●"
                                .with(random_bullet_color())
                                .attribute(crossterm::style::Attribute::Bold),
                            display.with(crossterm::style::Color::White),
                        );
                    }

                    // Execute (with permission check)
                    let exec_start = std::time::Instant::now();
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
                            tools::permission_denied_message(&tc.name, &reason, rules.as_deref())
                        }
                        crate::permissions::PermissionResult::Prompt => {
                            if permissions.prompt_tool_permission(&tc.name, &tc.arguments) {
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
                    let exec_duration = exec_start.elapsed();

                    // Collect for grouped summary display
                    let display_name = tools::extract_tool_display_name(&tc.name, &tc.arguments);
                    batch_tools.push((
                        tc.name.clone(),
                        display_name,
                        tc.arguments.clone(),
                        exec_duration,
                    ));

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
                        if let Some(group) = groups.iter_mut().find(|(n, _, _, _)| n == name) {
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
                            let args_map: std::collections::BTreeMap<String, serde_json::Value> =
                                serde_json::from_str(args_str).unwrap_or_default();
                            crate::ui::prompt::print_tool_call_summary(
                                name,
                                &args_map,
                                *first_duration,
                            );
                        } else {
                            // Multiple calls: grouped summary
                            let header = tools::format_tool_group_header(name, displays.len());
                            tools::print_tool_group(&header, displays, false);
                        }
                        println!();
                    }
                }

                // Continue the loop for the next LLM turn
                continue;
            }
            Err(e) => break Err(e),
        }
    };

    // Finalize any active Update group when the tool loop ends
    if let Some(ctx) = active_update.take() {
        finalize_oneshot_update(ctx);
        in_update_group.store(false, Ordering::Relaxed);
    }

    match result {
        Ok(()) => {
            if !final_text.is_empty() {
                let is_multi_line = final_text.trim().contains('\n');

                let (summary, displayed_text) =
                    if is_multi_line && config.enable_final_response_summary {
                        let summary_session_id = crate::api::session::resolve_chat_session_id(
                            &config.extra_headers,
                            &session_uuid,
                            crate::api::session::SessionKind::Summary,
                        );
                        let s = rt
                            .block_on(backend.summarize(&final_text, &summary_session_id))
                            .map(|(s, _)| s)
                            .unwrap_or_else(|_| "Response".to_string());
                        (s, final_text.clone())
                    } else {
                        crate::api::session::split_first_line_summary(&final_text)
                    };

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
        }
        Err(e) => {
            use crossterm::style::Stylize;
            println!(
                "{} {}",
                "●"
                    .with(random_bullet_color())
                    .attribute(crossterm::style::Attribute::Bold),
                "Error".with(crossterm::style::Color::Red),
            );
            if crate::api::client::is_model_error(&e) {
                let model = crate::ui::prompt::get_selected_model();
                let hint = match model {
                    Some(m) => format!(
                        "The model \"{}\" is not available. Use --model to specify a valid model, or omit it to use the server default.",
                        m,
                    ),
                    None => "No model is configured. Use --model to specify a model.".to_string(),
                };
                eprintln!(
                    "{} {}",
                    "└─".with(crossterm::style::Color::DarkGrey),
                    hint.as_str().with(crossterm::style::Color::Yellow),
                );
            } else {
                eprintln!(
                    "{} {}",
                    "└─".with(crossterm::style::Color::DarkGrey),
                    format!("{:#}", e).with(crossterm::style::Color::Yellow),
                );
            }
            std::process::exit(1);
        }
    }

    Ok(())
}
