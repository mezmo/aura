// ---------------------------------------------------------------------------
// Event log replay
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{Attribute, Stylize};

use crate::theme::{AuraStyle, Themed, theme};
use crossterm::terminal;

use crate::api::types::{DisplayEvent, snake_to_pascal_case};
use crate::tools;
use crate::ui::markdown::{render_markdown, render_summary};
use crate::ui::text::truncate_with_ellipsis;

use super::orchestrator::{
    TREE_END_BULLET, TREE_END_DURATION, TREE_MID_BULLET, TREE_MID_DURATION, format_orch_duration_ms,
};
use super::state::{
    EVENT_LOG, EXPANDED_OUTPUT, WELCOME_STATE, random_bullet_color, task_color_for,
};
use super::status_bar::{reset_status_bar_tokens, set_status_bar_tokens};

/// Clear the terminal and replay all recorded events.
pub fn replay_event_log_global() {
    let event_log = EVENT_LOG.lock().unwrap_or_else(|e| e.into_inner());
    let expanded = EXPANDED_OUTPUT.load(Ordering::Relaxed);
    let welcome = WELCOME_STATE.lock().unwrap_or_else(|e| e.into_inner());

    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0)
    );
    reset_status_bar_tokens();

    if let Some(ref w) = *welcome {
        w.print_static();
    }

    if expanded {
        println!("{}", "  [expanded view]".themed(AuraStyle::Muted),);
        println!();
    }

    let events = &*event_log;
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            DisplayEvent::UserInput(input) => {
                print_user_echo(input);
                println!();
                i += 1;
            }
            DisplayEvent::ToolCall { .. } => {
                let start = i;
                while i < events.len() {
                    if let DisplayEvent::ToolCall { .. } = &events[i] {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let group = &events[start..i];

                if expanded {
                    for event in group {
                        if let DisplayEvent::ToolCall {
                            tool_name,
                            arguments,
                            duration,
                            result,
                        } = event
                        {
                            print_tool_call_expanded(
                                tool_name,
                                arguments,
                                *duration,
                                result.as_deref(),
                            );
                        }
                    }
                } else {
                    #[allow(clippy::type_complexity)]
                    let mut groups: Vec<(
                        &str,
                        Vec<String>,
                        Option<&BTreeMap<String, serde_json::Value>>,
                        Option<Duration>,
                    )> = Vec::new();
                    for event in group {
                        if let DisplayEvent::ToolCall {
                            tool_name,
                            arguments,
                            duration,
                            ..
                        } = event
                        {
                            let args_json = serde_json::to_string(&serde_json::Value::Object(
                                arguments
                                    .iter()
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect(),
                            ))
                            .unwrap_or_default();
                            let display_name =
                                tools::extract_tool_display_name(tool_name, &args_json);
                            if let Some(g) = groups
                                .iter_mut()
                                .find(|(n, _, _, _)| *n == tool_name.as_str())
                            {
                                g.1.push(display_name);
                            } else {
                                groups.push((
                                    tool_name.as_str(),
                                    vec![display_name],
                                    Some(arguments),
                                    Some(*duration),
                                ));
                            }
                        }
                    }
                    for (name, displays, first_args, first_duration) in &groups {
                        if displays.len() == 1 {
                            if let Some(args) = first_args {
                                print_tool_call_summary(name, args, *first_duration);
                            }
                        } else {
                            let header = tools::format_tool_group_header(name, displays.len());
                            tools::print_tool_group(&header, displays, false);
                        }
                        println!();
                    }
                }
            }
            DisplayEvent::AssistantResponse { summary, text } => {
                if !summary.is_empty() {
                    render_summary(summary);
                }
                if !text.is_empty() {
                    println!();
                    render_markdown(text);
                }
                println!();
                i += 1;
            }
            DisplayEvent::Cancelled => {
                println!(
                    "{} {}",
                    "●".with(random_bullet_color()).attribute(Attribute::Bold),
                    "Interrupted (user requested)".attribute(Attribute::Bold),
                );
                println!(
                    "{} {}",
                    "└─".themed(AuraStyle::Connector),
                    "what should AURA do next?".themed(AuraStyle::Muted),
                );
                println!();
                i += 1;
            }
            DisplayEvent::Error(msg) => {
                println!(
                    "{} {}",
                    "●".with(random_bullet_color()).attribute(Attribute::Bold),
                    "Error".themed(AuraStyle::Error),
                );
                println!(
                    "{} {}",
                    "└─".themed(AuraStyle::Connector),
                    msg.as_str().themed(AuraStyle::Warning),
                );
                println!();
                i += 1;
            }
            DisplayEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                set_status_bar_tokens(*prompt_tokens, *completion_tokens);
                i += 1;
            }
            DisplayEvent::OrchestratorScratchpadSavings {
                tokens_intercepted,
                tokens_extracted,
            } => {
                super::status_bar::add_scratchpad_usage(*tokens_intercepted, *tokens_extracted);
                i += 1;
            }
            DisplayEvent::Compacted { messages_removed } => {
                println!(
                    "{} {}",
                    "●".with(random_bullet_color()).attribute(Attribute::Bold),
                    "Context Compacted".attribute(Attribute::Bold),
                );
                println!(
                    "{} {} messages removed",
                    "└─".themed(AuraStyle::Connector),
                    messages_removed,
                );
                println!();
                i += 1;
            }
            DisplayEvent::FileUpdate {
                file_path,
                commands_used,
                shell_calls,
                diff_text,
                lines_added,
                lines_removed,
                duration,
            } => {
                let time_str = if duration.as_secs_f64() < 1.0 {
                    format!("{:.0}ms", duration.as_secs_f64() * 1000.0)
                } else {
                    format!("{:.1}s", duration.as_secs_f64())
                };
                let header = tools::format_tool_group_header("Update", 1);
                println!(
                    "{} {}",
                    "●".with(random_bullet_color()).attribute(Attribute::Bold),
                    header.themed(AuraStyle::Primary),
                );

                println!(
                    "{} {}",
                    "├─".themed(AuraStyle::Connector),
                    file_path.as_str().themed(AuraStyle::Muted),
                );

                if expanded {
                    tools::print_update_commands_summary(commands_used, true, "├─");

                    let shell_count = shell_calls.len();
                    for (sc_idx, sc) in shell_calls.iter().enumerate() {
                        let sc_time = if sc.duration.as_secs_f64() < 1.0 {
                            format!("{:.0}ms", sc.duration.as_secs_f64() * 1000.0)
                        } else {
                            format!("{:.1}s", sc.duration.as_secs_f64())
                        };
                        let _is_last_shell = sc_idx == shell_count - 1;
                        println!(
                            "{} {} {}",
                            "├─".themed(AuraStyle::Connector),
                            "●".with(random_bullet_color()).attribute(Attribute::Bold),
                            format!("Shell({})", sc.full_command).themed(AuraStyle::Primary),
                        );
                        let result_lines: Vec<&str> = sc.result.lines().take(5).collect();
                        let has_more = sc.result.lines().count() > 5;
                        let total_sub = result_lines.len() + if has_more { 1 } else { 0 } + 1;
                        let mut sub_idx = 0;
                        for line in &result_lines {
                            sub_idx += 1;
                            let sub_conn = if sub_idx == total_sub {
                                "└─"
                            } else {
                                "├─"
                            };
                            println!(
                                "{}{} {}",
                                "│  ".themed(AuraStyle::Connector),
                                sub_conn.themed(AuraStyle::Connector),
                                line.themed(AuraStyle::Muted),
                            );
                        }
                        if has_more {
                            sub_idx += 1;
                            let sub_conn = if sub_idx == total_sub {
                                "└─"
                            } else {
                                "├─"
                            };
                            println!(
                                "{}{} {}",
                                "│  ".themed(AuraStyle::Connector),
                                sub_conn.themed(AuraStyle::Connector),
                                "… (truncated)".themed(AuraStyle::Muted),
                            );
                        }
                        println!(
                            "{}{} {} {}",
                            "│  ".themed(AuraStyle::Connector),
                            "└─".themed(AuraStyle::Connector),
                            "completed".themed(AuraStyle::Muted),
                            format!("({sc_time})").themed(AuraStyle::Muted),
                        );
                    }
                    tools::print_update_summary(*lines_added, *lines_removed, "├─");
                    tools::print_update_diff(diff_text, 0);
                } else {
                    tools::print_update_summary(*lines_added, *lines_removed, "├─");
                    tools::print_update_diff(diff_text, 10);
                }

                println!(
                    "{} {} {}",
                    "└─".themed(AuraStyle::Connector),
                    "tool completed".themed(AuraStyle::Muted),
                    format!("({time_str})").themed(AuraStyle::Muted),
                );
                println!();
                i += 1;
            }
            DisplayEvent::OrchestratorPlanCreated { goal, fields } => {
                let bc = task_color_for("__orchestrator__");
                println!(
                    "{} {}",
                    "●".with(bc).attribute(Attribute::Bold),
                    format!("Plan - {goal}").attribute(Attribute::Bold),
                );
                if expanded {
                    print_fields_tree(fields);
                }
                println!();
                i += 1;
            }
            DisplayEvent::OrchestratorTaskStarted {
                task_id,
                worker_id,
                description,
                ..
            } => {
                // Collect every entry that belongs inside this task tree:
                // tool calls and worker-reasoning blocks (whose `agent_id`
                // matches the task's `worker_id`). Reasoning for a different
                // agent ends the inner walk so the outer loop can render it
                // separately.
                enum TaskEntry {
                    Tool {
                        label: String,
                        fields: BTreeMap<String, serde_json::Value>,
                        duration_ms: Option<u64>,
                        result: Option<String>,
                    },
                    Reasoning {
                        content: String,
                        fields: BTreeMap<String, serde_json::Value>,
                    },
                }
                let mut entries: Vec<TaskEntry> = Vec::new();
                let mut last_reasoning: Option<String> = None;
                let mut j = i + 1;
                while j < events.len() {
                    match &events[j] {
                        DisplayEvent::OrchestratorToolCallStarted {
                            tool_name, fields, ..
                        } => {
                            let dn = snake_to_pascal_case(tool_name);
                            let args = format_orch_args_summary(fields);
                            entries.push(TaskEntry::Tool {
                                label: format!("{dn}({args})"),
                                fields: fields.clone(),
                                duration_ms: None,
                                result: None,
                            });
                            if let Some(r) = extract_aura_reasoning(fields) {
                                last_reasoning = Some(r);
                            }
                            j += 1;
                        }
                        DisplayEvent::OrchestratorToolCallCompleted {
                            duration_ms,
                            fields,
                            ..
                        } => {
                            if let Some(TaskEntry::Tool {
                                duration_ms: dm,
                                result: r,
                                ..
                            }) = entries.last_mut()
                            {
                                *dm = *duration_ms;
                                *r = fields.get("result").and_then(|v| match v {
                                    serde_json::Value::String(s) => Some(s.clone()),
                                    _ => None,
                                });
                            }
                            j += 1;
                        }
                        DisplayEvent::Reasoning {
                            content,
                            agent_id,
                            fields,
                        } if agent_id == worker_id => {
                            entries.push(TaskEntry::Reasoning {
                                content: content.clone(),
                                fields: fields.clone(),
                            });
                            last_reasoning = Some(content.clone());
                            j += 1;
                        }
                        DisplayEvent::Reasoning { .. } => {
                            // Reasoning for a different agent — bail so the
                            // outer loop renders it separately. Don't advance
                            // past the task_completed event yet either.
                            break;
                        }
                        DisplayEvent::OrchestratorTaskCompleted { .. } => {
                            j += 1;
                            break;
                        }
                        _ => break,
                    }
                }
                let bc = task_color_for(task_id);
                if !expanded {
                    let reasoning_text = last_reasoning.as_deref().or(if description.is_empty() {
                        None
                    } else {
                        Some(description.as_str())
                    });
                    if let Some(text) = reasoning_text {
                        let display = truncate_with_ellipsis(text, 120);
                        println!(
                            "{} {}",
                            "●".with(bc).attribute(Attribute::Bold),
                            display.themed(AuraStyle::Primary),
                        );
                        println!();
                    }
                }
                println!(
                    "{} {} {} {} {} {}",
                    "●".with(bc).attribute(Attribute::Bold),
                    format!("Task {task_id}").attribute(Attribute::Bold),
                    "-".themed(AuraStyle::Muted),
                    format!("Worker: {worker_id}").themed(AuraStyle::Muted),
                    "-".themed(AuraStyle::Muted),
                    "done".themed(AuraStyle::Muted),
                );
                let entry_count = entries.len();
                for (idx, entry) in entries.iter().enumerate() {
                    let is_last = idx == entry_count - 1;
                    let (b_prefix, cont_prefix) = if is_last {
                        (TREE_END_BULLET, TREE_END_DURATION)
                    } else {
                        (TREE_MID_BULLET, TREE_MID_DURATION)
                    };
                    match entry {
                        TaskEntry::Tool {
                            label,
                            fields: tool_fields,
                            duration_ms,
                            result,
                        } => {
                            println!(
                                "{}{} {}",
                                b_prefix.themed(AuraStyle::Connector),
                                "●".with(bc),
                                label.as_str().themed(AuraStyle::Primary),
                            );
                            if expanded {
                                let has_duration = duration_ms.is_some();
                                let has_fields = !tool_fields.is_empty();
                                if let Some(ms) = duration_ms {
                                    let dur_str = format_orch_duration_ms(*ms);
                                    let item_prefix = if has_fields { "├─" } else { "└─" };
                                    println!(
                                        "{}{} {}",
                                        cont_prefix.themed(AuraStyle::Connector),
                                        item_prefix.themed(AuraStyle::Connector),
                                        format!("completed in {dur_str}")
                                            .as_str()
                                            .themed(AuraStyle::Muted),
                                    );
                                }
                                if has_fields {
                                    super::orchestrator::print_fields_tree_indented(
                                        tool_fields,
                                        cont_prefix,
                                        has_duration,
                                    );
                                }
                                if let Some(text) = result.as_deref()
                                    && !text.is_empty()
                                {
                                    let normalized = crate::tools::normalize_tool_result_text(text);
                                    println!();
                                    for line in normalized.lines() {
                                        println!(
                                            "{}  {}",
                                            cont_prefix.themed(AuraStyle::Connector),
                                            line.themed(AuraStyle::Muted),
                                        );
                                    }
                                    println!();
                                }
                            } else if let Some(ms) = duration_ms {
                                let dur_str = format_orch_duration_ms(*ms);
                                println!(
                                    "{}{} {}",
                                    cont_prefix.themed(AuraStyle::Connector),
                                    "⎿".themed(AuraStyle::Connector),
                                    format!("completed in {dur_str}")
                                        .as_str()
                                        .themed(AuraStyle::Muted),
                                );
                            }
                        }
                        TaskEntry::Reasoning {
                            content,
                            fields: reasoning_fields,
                        } => {
                            println!(
                                "{}{} {}",
                                b_prefix.themed(AuraStyle::Connector),
                                "●".with(bc),
                                "Reasoning".themed(AuraStyle::Primary),
                            );
                            // Worker reasoning body is a single line in the
                            // live display; flatten whitespace and truncate
                            // for replay so we match what the user saw.
                            let flattened: String =
                                content.split_whitespace().collect::<Vec<_>>().join(" ");
                            let display = if flattened.chars().count() > 200 {
                                let prefix: String = flattened.chars().take(197).collect();
                                format!("{prefix}...")
                            } else {
                                flattened
                            };
                            println!(
                                "{}{} {}",
                                cont_prefix.themed(AuraStyle::Connector),
                                "⎿".themed(AuraStyle::Connector),
                                display.as_str().themed(AuraStyle::Muted),
                            );
                            if expanded && !reasoning_fields.is_empty() {
                                super::orchestrator::print_fields_tree_indented(
                                    reasoning_fields,
                                    cont_prefix,
                                    false,
                                );
                            }
                        }
                    }
                }
                println!();
                i = j;
            }
            DisplayEvent::OrchestratorToolCallStarted { .. } => {
                i += 1;
            }
            DisplayEvent::OrchestratorToolCallCompleted { .. } => {
                i += 1;
            }
            DisplayEvent::OrchestratorTaskCompleted { .. } => {
                i += 1;
            }
            DisplayEvent::OrchestratorSynthesizing => {
                i += 1;
            }
            DisplayEvent::Reasoning {
                content,
                agent_id,
                fields,
            } => {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    let bc = task_color_for(if agent_id.is_empty() {
                        "__orchestrator__"
                    } else {
                        agent_id.as_str()
                    });
                    let header = if agent_id == "main" || agent_id.is_empty() {
                        "Reasoning".to_string()
                    } else {
                        format!("Reasoning - {agent_id}")
                    };
                    println!(
                        "{} {}",
                        "●".with(bc).attribute(Attribute::Bold),
                        header
                            .as_str()
                            .themed(AuraStyle::Primary)
                            .attribute(Attribute::Bold),
                    );
                    if expanded {
                        // Show wire-level fields under the header (agent_id,
                        // content, parent_agent_id, session_id, trace_id) so
                        // /expand mirrors how other orchestrator events render
                        // their fields. `content` in the tree is the full
                        // accumulated reasoning text — we override the map's
                        // value here just in case the persisted entry only
                        // captured a single delta.
                        let mut tree = fields.clone();
                        tree.insert(
                            "content".to_string(),
                            serde_json::Value::String(content.clone()),
                        );
                        print_fields_tree(&tree);
                    } else {
                        for line in trimmed.lines() {
                            println!(
                                "{} {}",
                                "│".themed(AuraStyle::Connector),
                                line.themed(AuraStyle::Muted),
                            );
                        }
                    }
                    println!();
                }
                i += 1;
            }
            DisplayEvent::OrchestratorIterationComplete {
                iteration,
                timing_line,
                fields,
            } => {
                let bc = task_color_for("__orchestrator__");
                println!(
                    "{} {}",
                    "●".with(bc).attribute(Attribute::Bold),
                    "Iteration complete".attribute(Attribute::Bold),
                );
                let has_fields = expanded && !fields.is_empty();
                // Order mirrors the live render in `repl/loop.rs`: timing first,
                // iteration last, so both paths look identical.
                println!(
                    "{} timing: {}",
                    "├─".themed(AuraStyle::Connector),
                    timing_line.as_str().themed(AuraStyle::Muted),
                );
                let iteration_connector = if has_fields { "├─" } else { "└─" };
                println!(
                    "{} iteration: {}",
                    iteration_connector.themed(AuraStyle::Connector),
                    iteration.to_string().as_str().themed(AuraStyle::Muted),
                );
                if has_fields {
                    print_fields_tree(fields);
                }
                println!();
                i += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Replay helper functions
// ---------------------------------------------------------------------------

/// Parse the `arguments` sub-object from an orchestrator event's fields map.
fn parse_orch_arguments(
    fields: &BTreeMap<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    fields.get("arguments").and_then(|a| match a {
        serde_json::Value::Object(obj) => Some(obj.clone()),
        serde_json::Value::String(s) => {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(s).ok()
        }
        _ => None,
    })
}

fn format_orch_args_summary(fields: &BTreeMap<String, serde_json::Value>) -> String {
    match parse_orch_arguments(fields) {
        Some(obj) => obj
            .iter()
            .filter(|(k, v)| {
                !k.starts_with('_')
                    && !matches!(v, serde_json::Value::Null)
                    && !matches!(v, serde_json::Value::String(s) if s.is_empty() || s == "null")
            })
            .take(3)
            .map(|(k, v)| {
                let val_str = match v {
                    serde_json::Value::String(s) => {
                        if s.len() > 20 {
                            format!("\"{}...\"", &s[..17])
                        } else {
                            format!("\"{s}\"")
                        }
                    }
                    other => {
                        let s = other.to_string();
                        if s.len() > 20 {
                            format!("{}...", &s[..17])
                        } else {
                            s
                        }
                    }
                };
                format!("{k}: {val_str}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

fn extract_aura_reasoning(fields: &BTreeMap<String, serde_json::Value>) -> Option<String> {
    parse_orch_arguments(fields).and_then(|obj| {
        obj.get("_aura_reasoning")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })
}

/// Print a `BTreeMap` of fields as an indented tree for `/expand` display.
pub fn print_fields_tree(fields: &BTreeMap<String, serde_json::Value>) {
    if fields.is_empty() {
        return;
    }
    let total = fields.len();
    for (idx, (key, val)) in fields.iter().enumerate() {
        let is_last = idx == total - 1;
        let connector = if is_last { "└─" } else { "├─" };
        match val {
            serde_json::Value::Object(obj) => {
                println!(
                    "{} {}:",
                    connector.themed(AuraStyle::Connector),
                    key.as_str().themed(AuraStyle::Muted),
                );
                let child_cont = if is_last { "   " } else { "│  " };
                let sub_total = obj.len();
                for (sub_idx, (sub_key, sub_val)) in obj.iter().enumerate() {
                    let sub_is_last = sub_idx == sub_total - 1;
                    let sub_connector = if sub_is_last { "└─" } else { "├─" };
                    let val_str = match sub_val {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    println!(
                        "{}{} {}: {}",
                        child_cont.themed(AuraStyle::Connector),
                        sub_connector.themed(AuraStyle::Connector),
                        sub_key.as_str().themed(AuraStyle::Muted),
                        val_str.as_str().themed(AuraStyle::Muted),
                    );
                }
            }
            _ => {
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                println!(
                    "{} {}: {}",
                    connector.themed(AuraStyle::Connector),
                    key.as_str().themed(AuraStyle::Muted),
                    val_str.as_str().themed(AuraStyle::Muted),
                );
            }
        }
    }
}

/// Echo the user's prompt with a dark background bar.
pub fn print_user_echo(input: &str) {
    let (width, _) = super::state::term_size();
    let content = format!("❯ {}", input);
    let padded = format!("{:<width$}", content, width = width as usize);
    println!(
        "{}",
        padded
            .themed(AuraStyle::UserEchoFg)
            .on(theme().user_echo_bg),
    );
}

// ---------------------------------------------------------------------------
// Replay renderers (compact / expanded tool calls)
// ---------------------------------------------------------------------------

/// Render a single-agent tool call in compact form, matching orchestrator style
/// (without task tree connectors):
///
/// ```text
/// ● Head(file: "task_1...", lines: 60)
/// ⎿ completed in 17ms
/// ```
///
/// If `duration` is `None` (e.g. local tools where timing isn't tracked), the
/// `⎿ completed in …` line is omitted.
pub fn print_tool_call_summary(
    tool_name: &str,
    args: &std::collections::BTreeMap<String, serde_json::Value>,
    duration: Option<Duration>,
) {
    let label = crate::tools::format_tool_call_label(tool_name, args);
    println!(
        "{} {}",
        "●".with(random_bullet_color()).attribute(Attribute::Bold),
        label.themed(AuraStyle::Primary),
    );
    if let Some(d) = duration {
        let dur_str = super::orchestrator::format_orch_duration_ms(d.as_millis() as u64);
        println!(
            "{} {}",
            "⎿".themed(AuraStyle::Connector),
            format!("completed in {dur_str}")
                .as_str()
                .themed(AuraStyle::Muted),
        );
    }
}

/// Render a single-agent tool call in expanded form, matching orchestrator style:
///
/// ```text
/// ● Head(file: "task_1...", lines: 60)
/// ├─ completed in 17ms
/// ├─ tool_name: Head
/// └─ arguments:
///    ├─ file: ...
///    └─ lines: 60
///
///    <tool output content>
/// ```
pub fn print_tool_call_expanded(
    tool_name: &str,
    args: &std::collections::BTreeMap<String, serde_json::Value>,
    duration: Duration,
    result: Option<&str>,
) {
    let label = crate::tools::format_tool_call_label(tool_name, args);
    println!(
        "{} {}",
        "●".with(random_bullet_color()).attribute(Attribute::Bold),
        label.themed(AuraStyle::Primary),
    );

    let dur_str = super::orchestrator::format_orch_duration_ms(duration.as_millis() as u64);
    let has_args = !args.is_empty();

    println!(
        "{} {}",
        "├─".themed(AuraStyle::Connector),
        format!("completed in {dur_str}")
            .as_str()
            .themed(AuraStyle::Muted),
    );
    let tool_name_connector = if has_args { "├─" } else { "└─" };
    println!(
        "{} tool_name: {}",
        tool_name_connector.themed(AuraStyle::Connector),
        tool_name.themed(AuraStyle::Muted),
    );

    if has_args {
        println!("{} arguments:", "└─".themed(AuraStyle::Connector));
        let total = args.len();
        for (idx, (key, val)) in args.iter().enumerate() {
            let is_last = idx == total - 1;
            let connector = if is_last { "└─" } else { "├─" };
            let val_str = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            println!(
                "   {} {}: {}",
                connector.themed(AuraStyle::Connector),
                key.as_str().themed(AuraStyle::Muted),
                val_str.as_str().themed(AuraStyle::Muted),
            );
        }
    }

    if let Some(text) = result
        && !text.is_empty()
    {
        let normalized = crate::tools::normalize_tool_result_text(text);
        println!();
        for line in normalized.lines() {
            println!(
                "{}  {}",
                "   ".themed(AuraStyle::Connector),
                line.themed(AuraStyle::Muted),
            );
        }
    }
    println!();
}

pub fn print_help() {
    use crate::repl::registry::COMMANDS;
    println!("Available commands:");
    let width = COMMANDS
        .iter()
        .map(|c| c.name.len() + c.usage_hint.map_or(0, |h| h.len() + 1))
        .max()
        .unwrap_or(0);
    for cmd in COMMANDS {
        let cell = match cmd.usage_hint {
            Some(hint) => format!("{} {}", cmd.name, hint),
            None => cmd.name.to_string(),
        };
        println!("  {cell:<width$} — {}", cmd.description);
    }
}

pub fn list_conversations() {
    let convos = crate::repl::conversations::ConversationStore::list_all();
    if convos.is_empty() {
        println!("No saved conversations.");
        return;
    }
    println!("Saved conversations:");
    println!();
    for (uuid, name) in &convos {
        let short_id = &uuid[..8.min(uuid.len())];
        let display_name = if name.is_empty() {
            "(untitled)"
        } else {
            name.trim()
        };
        println!(
            "  {} {}",
            short_id.themed(AuraStyle::Identifier),
            display_name.themed(AuraStyle::Primary),
        );
    }
    println!();
    println!(
        "{}",
        "Use /resume <id> to continue a conversation.".themed(AuraStyle::Muted),
    );
}
