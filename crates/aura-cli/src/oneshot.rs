//! One-shot mode: run a single `--query`, print the model's response to
//! stdout, exit.
//!
//! ## Output contract
//!
//! Stdout in one-shot mode contains **only the raw assistant response**.
//! No bullet markers, no styled headers, no tool-execution summaries, no
//! markdown rendering. The output is the verbatim text the model produced
//! so callers can pipe it (`aura --query ... | jq`, `... | tee`,
//! `... > out.md`) without scrubbing decorations off.
//!
//! Everything else — tracing logs (when `--log-file` is set), permission
//! prompts, error messages, and missing-server-tool warnings — goes to
//! **stderr** so the pipe stays clean. Use `2>/dev/null` to silence those
//! channels when running unattended.
//!
//! The interactive REPL keeps its rich formatting (see `repl::r#loop`).
//! Decisions about markers, colours, and markdown are deliberately
//! scoped to the REPL there and intentionally absent here.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use tokio::runtime::Runtime;

use crate::api::mcp_status::{McpNotice, notices_from_event};
use crate::api::stream::{StreamHandler, StreamResult};
use crate::api::types::ToolCallInfo;
use crate::backend::Backend;
use crate::config::AppConfig;
use crate::repl::history::ConversationHistory;
use crate::tools;
use crate::ui::prompt::set_selected_model;

/// One-shot [`StreamHandler`] that ignores every event except
/// `aura.mcp_status`, which it renders to **stderr** so degraded MCP servers
/// are visible without polluting stdout (reserved for the assistant response
/// per the output contract).
struct OneshotStreamHandler;

impl StreamHandler for OneshotStreamHandler {
    fn on_orchestrator_event(&mut self, event_name: &str, value: &serde_json::Value) {
        if event_name != "aura.mcp_status" {
            return;
        }
        for notice in notices_from_event(value) {
            let (prefix, message) = match &notice {
                McpNotice::Error(message) => ("error:", message),
                McpNotice::Warning(message) => ("warning:", message),
            };
            eprintln!("{prefix} {message}");
        }
    }
}

/// `rt` is the CLI's process-wide tokio runtime, owned by `main`. We
/// don't build our own here — sharing the runtime with `main`'s OTel
/// setup means traces emitted during a one-shot request use the same
/// `BatchSpanProcessor` worker that `main` registered.
pub fn run_oneshot(
    rt: &Runtime,
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

    // One-shot has no ConversationStore — generate a single process-lifetime
    // UUID so all turns within this invocation share a chat session.
    let session_uuid = uuid::Uuid::new_v4().to_string();
    let chat_session_id = crate::api::session::resolve_chat_session_id(
        &config.extra_headers,
        &session_uuid,
        crate::api::session::SessionKind::Chat,
    );

    // Tool execution loop. One-shot ignores nearly every stream event —
    // stdout is reserved for the final assistant text — but degraded MCP
    // servers are surfaced on stderr via `OneshotStreamHandler`.
    let result: Result<()> = loop {
        let stream_result = rt.block_on(async {
            backend
                .stream_chat(
                    conversation.messages(),
                    tool_defs_arg,
                    &chat_session_id,
                    Arc::new(AtomicBool::new(false)),
                    &mut OneshotStreamHandler,
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

                let text_content = if text.is_empty() { None } else { Some(text) };
                conversation.add_assistant_with_tool_calls(text_content, tool_call_infos);

                for tc in &tool_calls {
                    // Server-side tools (MCP, RAG, etc.) ran on the agent
                    // side; their results arrive on `aura.tool_complete`
                    // events and are buffered into `server_results`. We
                    // simply forward them into the conversation so the
                    // next turn sees the tool output.
                    if !tools::is_local_tool(&tc.name) {
                        let result = match server_results.get(&tc.id).cloned() {
                            Some(r) => r,
                            None => {
                                // Plain stderr line — no markers, no
                                // theming. Pipe consumers won't see this.
                                eprintln!(
                                    "warning: no result for server tool '{}' \
                                     (set AURA_CUSTOM_EVENTS=true on the server)",
                                    tc.name
                                );
                                tools::missing_server_result_message(&tc.name)
                            }
                        };
                        conversation.add_tool_result(&tc.id, &tc.name, &result);
                        continue;
                    }

                    // Local tool execution. Permission prompts and warnings
                    // emit on stderr inside `PermissionChecker`, so stdout
                    // remains untouched. The result text is fed back to
                    // the model, never printed here.
                    let perm = permissions.check(&tc.name, &tc.arguments);
                    let tool_result = match perm {
                        crate::permissions::PermissionResult::Allowed => {
                            tools::execute_tool(&tc.name, &tc.arguments)
                                .unwrap_or_else(|e| format!("Error: {e}"))
                        }
                        crate::permissions::PermissionResult::Denied(reason) => {
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

                    conversation.add_tool_result(&tc.id, &tc.name, &tool_result);
                }

                continue;
            }
            Err(e) => break Err(e),
        }
    };

    match result {
        Ok(()) => {
            // Raw assistant text to stdout, exactly as the server emitted
            // it. A trailing newline is appended only when the response
            // doesn't already end with one — same convention as `curl`,
            // `cat`, etc., so callers piping into shells get a clean line
            // break without doubling up on multi-line responses.
            if !final_text.is_empty() {
                print!("{final_text}");
                if !final_text.ends_with('\n') {
                    println!();
                }
            }
        }
        Err(e) => {
            // Errors go to stderr only — stdout stays empty so callers
            // can rely on "exit code 0 ⇒ stdout is the response, exit
            // code != 0 ⇒ stderr explains why".
            if crate::api::client::is_model_error(&e) {
                let model = crate::ui::prompt::get_selected_model();
                let hint = match model {
                    Some(m) => format!(
                        "The model \"{m}\" is not available. Use --model to specify a valid \
                         model, or omit it to use the server default."
                    ),
                    None => "No model is configured. Use --model to specify a model.".to_string(),
                };
                eprintln!("error: {hint}");
            } else {
                eprintln!("error: {e:#}");
            }
            std::process::exit(1);
        }
    }

    Ok(())
}
