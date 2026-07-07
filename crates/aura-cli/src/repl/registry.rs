//! Slash-command registry. [`COMMANDS`] is the list of REPL commands that
//! dispatch (`repl/loop.rs`), Enter-validation (`ui/input_hint.rs`), `/help`
//! (`ui/event_replay.rs`), and autocomplete all read from.
//!
//! Each [`Command`] carries its name, help text, a [`CommandFn`] handler, and
//! an optional submission gate. A handler takes a [`CommandContext`] and the
//! argument string and returns a [`CommandOutcome`] directing the REPL loop.

use rustyline::Editor;
use rustyline::history::DefaultHistory;

use super::commands;
use super::conversations::ConversationStore;
use super::history::ConversationHistory;
use super::input_reader::AuraHelper;
use crate::backend::Backend;
use crate::ui::prompt::{is_expanded_output, with_event_log};
use crate::ui::state::{MODEL_MATCHES, RESUME_MATCHES, STYLE_MATCHES, get_tab_select_index};

/// Mutable per-session state passed to each command handler.
pub(crate) struct CommandContext<'a> {
    pub conversation: &'a mut ConversationHistory,
    pub conv_store: &'a mut Option<ConversationStore>,
    pub input_reader: &'a mut Editor<AuraHelper, DefaultHistory>,
    /// Telemetry handle, so `/telemetry status|recent|disable` can read
    /// state and persist an opt-out. Inspection/disable only — dispatch
    /// happens before the consent gate, so a slash command never enables.
    pub telemetry: &'a aura_telemetry::TelemetryHandle,
    pub rt: &'a tokio::runtime::Runtime,
    pub backend: &'a Backend,
}

/// What the REPL loop should do after a command handler returns.
pub(crate) enum CommandOutcome {
    /// Command handled itself (drew its own output); read the next input.
    Handled,
    /// Pre-fill the next readline with this text (e.g. a resumed
    /// conversation's pending input).
    Reinject(String),
    /// Tear down and leave the REPL loop.
    Exit,
}

/// A command resolved against [`COMMANDS`], paired with its argument string,
/// ready to run without re-parsing. Used to hand a command typed mid-stream to
/// the main loop, where the full session state needed to run it is available.
pub(crate) struct PendingCommand {
    pub command: &'static Command,
    pub args: String,
}

/// Uniform handler signature. `args` is the trimmed remainder after the
/// (resolved) command word.
pub(crate) type CommandFn = fn(&mut CommandContext, &str) -> CommandOutcome;

/// Optional per-command Enter-submission gate. Receives the resolved, trimmed
/// input line. Returns `false` to keep rustyline from submitting (e.g. an
/// ambiguous `/resume` filter). Commands without a gate are always
/// submittable.
pub(crate) type ValidateFn = fn(&str) -> bool;

/// How a command behaves when typed while a response is streaming.
pub(crate) enum MidStream {
    /// Run immediately without interrupting the stream. The handler receives
    /// the post-command argument string and touches only global display state.
    Live(fn(&str)),
    /// Cancel the stream and re-run the command line in the main loop, where
    /// the full session state (and `handler`) is available.
    Defer,
}

/// A single REPL slash command.
pub(crate) struct Command {
    pub name: &'static str,
    pub description: &'static str,
    /// Argument placeholder for `/help` (e.g. `"<filter>"`); `None` for
    /// commands that take no argument.
    pub usage_hint: Option<&'static str>,
    pub handler: CommandFn,
    pub validate: Option<ValidateFn>,
    pub mid_stream: MidStream,
}

/// The command an interrupt (Ctrl-C / SIGINT) defers to tear down the REPL.
/// Named so those sites can take a direct `&'static` reference to it (via
/// const promotion) instead of a fallible name lookup.
pub(crate) const QUIT_COMMAND: Command = Command {
    name: "/quit",
    description: "Exit the REPL",
    usage_hint: None,
    handler: cmd_exit,
    validate: None,
    mid_stream: MidStream::Defer,
};

pub(crate) const COMMANDS: &[Command] = &[
    QUIT_COMMAND,
    Command {
        name: "/exit",
        description: "Exit the REPL",
        usage_hint: None,
        handler: cmd_exit,
        validate: None,
        mid_stream: MidStream::Defer,
    },
    Command {
        name: "/clear",
        description: "Start a new conversation",
        usage_hint: None,
        handler: cmd_clear,
        validate: None,
        mid_stream: MidStream::Defer,
    },
    Command {
        name: "/help",
        description: "Show this help message",
        usage_hint: None,
        handler: cmd_help,
        validate: None,
        mid_stream: MidStream::Live(crate::ui::mid_stream::live_help),
    },
    Command {
        name: "/expand",
        description: "Toggle expanded/compact tool call view",
        usage_hint: None,
        handler: cmd_expand,
        validate: None,
        mid_stream: MidStream::Live(crate::ui::mid_stream::live_expand),
    },
    Command {
        name: "/stream",
        description: "Toggle SSE event stream panel",
        usage_hint: None,
        handler: cmd_stream,
        validate: None,
        mid_stream: MidStream::Live(crate::ui::mid_stream::live_stream),
    },
    Command {
        name: "/conversations",
        description: "List saved conversations",
        usage_hint: None,
        handler: cmd_conversations,
        validate: None,
        mid_stream: MidStream::Live(crate::ui::mid_stream::live_conversations),
    },
    Command {
        name: "/resume",
        description: "Resume a saved conversation (by ID or name)",
        usage_hint: Some("<filter>"),
        handler: cmd_resume,
        validate: Some(validate_resume),
        mid_stream: MidStream::Defer,
    },
    Command {
        name: "/rename",
        description: "Rename the current conversation",
        usage_hint: Some("<name>"),
        handler: cmd_rename,
        validate: None,
        mid_stream: MidStream::Defer,
    },
    Command {
        name: "/model",
        description: "Select a model for LLM requests",
        usage_hint: Some("<filter>"),
        handler: cmd_model,
        validate: Some(validate_model),
        mid_stream: MidStream::Live(crate::ui::mid_stream::live_model),
    },
    Command {
        name: "/style",
        description: "Switch the visual style",
        usage_hint: Some("<name>"),
        handler: cmd_style,
        validate: Some(validate_style),
        mid_stream: MidStream::Live(crate::ui::mid_stream::live_style),
    },
    Command {
        name: "/telemetry",
        description: "Inspect or disable anonymous usage telemetry",
        usage_hint: Some("status|recent|disable"),
        handler: cmd_telemetry,
        validate: None,
        mid_stream: MidStream::Defer,
    },
];

/// Look up a command by its exact, fully-resolved name.
pub(crate) fn lookup(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// Split a resolved command line into its command word (the leading
/// whitespace-delimited token) and the trimmed argument remainder.
pub(crate) fn split_command(resolved: &str) -> (&str, &str) {
    let word = resolved.split_whitespace().next().unwrap_or(resolved);
    let args = resolved.strip_prefix(word).map(str::trim).unwrap_or("");
    (word, args)
}

/// Run a command line through its handler, returning the outcome. `None` means
/// the line names no known command.
pub(crate) fn dispatch(line: &str, ctx: &mut CommandContext) -> Option<CommandOutcome> {
    let (word, args) = split_command(line);
    Some((lookup(word)?.handler)(ctx, args))
}

fn cmd_exit(ctx: &mut CommandContext, _args: &str) -> CommandOutcome {
    // Save before exiting, or delete if the conversation was never started.
    if let Some(store) = ctx.conv_store.as_ref() {
        if ctx.conversation.messages().len() > 1 {
            with_event_log(|log| {
                store.save_all(ctx.conversation.messages(), log, is_expanded_output())
            });
        } else {
            store.delete();
        }
    }
    CommandOutcome::Exit
}

fn cmd_clear(ctx: &mut CommandContext, _args: &str) -> CommandOutcome {
    commands::handle_clear(ctx.conversation, ctx.conv_store, ctx.input_reader);
    CommandOutcome::Handled
}

fn cmd_help(_ctx: &mut CommandContext, _args: &str) -> CommandOutcome {
    commands::handle_help();
    CommandOutcome::Handled
}

fn cmd_expand(ctx: &mut CommandContext, _args: &str) -> CommandOutcome {
    commands::handle_expand(ctx.conversation, ctx.conv_store);
    CommandOutcome::Handled
}

fn cmd_stream(_ctx: &mut CommandContext, _args: &str) -> CommandOutcome {
    commands::handle_stream();
    CommandOutcome::Handled
}

fn cmd_conversations(_ctx: &mut CommandContext, _args: &str) -> CommandOutcome {
    commands::handle_conversations();
    CommandOutcome::Handled
}

fn cmd_resume(ctx: &mut CommandContext, args: &str) -> CommandOutcome {
    // In-REPL resume ignores CLI --model/--system-prompt and uses whatever the
    // resumed conversation had saved.
    match commands::handle_resume(
        args,
        ctx.conversation,
        ctx.conv_store,
        ctx.input_reader,
        None,
    ) {
        Some(new_input) => CommandOutcome::Reinject(new_input),
        None => CommandOutcome::Handled,
    }
}

fn cmd_rename(ctx: &mut CommandContext, args: &str) -> CommandOutcome {
    commands::handle_rename(args, ctx.conv_store);
    CommandOutcome::Handled
}

fn cmd_model(ctx: &mut CommandContext, args: &str) -> CommandOutcome {
    commands::handle_model(args, ctx.conv_store, ctx.rt, ctx.backend);
    CommandOutcome::Handled
}

fn cmd_style(_ctx: &mut CommandContext, args: &str) -> CommandOutcome {
    commands::handle_style(args);
    CommandOutcome::Handled
}

fn cmd_telemetry(ctx: &mut CommandContext, args: &str) -> CommandOutcome {
    commands::handle_telemetry(args, ctx.telemetry);
    CommandOutcome::Handled
}

/// Block Enter while a command's argument is still ambiguous against the live
/// autocomplete match state.
fn validate_resume(_input: &str) -> bool {
    if get_tab_select_index().is_some() {
        return true;
    }
    RESUME_MATCHES.lock().map(|g| g.len()).unwrap_or(0) == 1
}

fn validate_model(input: &str) -> bool {
    if get_tab_select_index().is_some() {
        return true;
    }
    let filter = input.trim().strip_prefix("/model").unwrap_or("").trim();
    if filter.is_empty() {
        MODEL_MATCHES.lock().map(|g| g.len()).unwrap_or(0) == 1
    } else {
        true
    }
}

fn validate_style(input: &str) -> bool {
    if get_tab_select_index().is_some() {
        return true;
    }
    let filter = input.trim().strip_prefix("/style").unwrap_or("").trim();
    let count = STYLE_MATCHES.lock().map(|g| g.len()).unwrap_or(0);
    !filter.is_empty() && count == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_look_up() {
        for cmd in COMMANDS {
            assert!(lookup(cmd.name).is_some(), "{} not found", cmd.name);
        }
    }

    #[test]
    fn ungated_commands_submit() {
        for cmd in COMMANDS.iter().filter(|c| c.validate.is_none()) {
            assert!(
                crate::ui::input_hint::validate_command_input(cmd.name),
                "{} is not submittable",
                cmd.name
            );
        }
    }

    #[test]
    fn lookup_is_exact() {
        assert!(lookup("/he").is_none());
        assert!(lookup("/nope").is_none());
    }

    #[test]
    fn resume_gate() {
        crate::ui::state::set_tab_select_index(None);
        RESUME_MATCHES.lock().unwrap().clear();
        assert!(!validate_resume("/resume foo"));
        *RESUME_MATCHES.lock().unwrap() = vec![("id".into(), "name".into())];
        assert!(validate_resume("/resume foo"));
        RESUME_MATCHES.lock().unwrap().clear();
    }

    #[test]
    fn style_gate() {
        crate::ui::state::set_tab_select_index(None);
        *STYLE_MATCHES.lock().unwrap() = vec!["dark".into()];
        assert!(validate_style("/style dark"));
        assert!(!validate_style("/style")); // no filter
        STYLE_MATCHES.lock().unwrap().clear();
        assert!(!validate_style("/style dark")); // no match
    }

    #[test]
    fn model_gate() {
        use crate::ui::input_hint::validate_command_input;
        crate::ui::state::set_tab_select_index(None);
        // Empty filter requires exactly one match.
        MODEL_MATCHES.lock().unwrap().clear();
        assert!(!validate_model("/model"));
        *MODEL_MATCHES.lock().unwrap() = vec!["gpt-4".into()];
        assert!(validate_model("/model"));
        // The gate is reached through validate_command_input by exact name.
        assert!(validate_command_input("/model"));
        MODEL_MATCHES.lock().unwrap().clear();
        assert!(!validate_command_input("/model"));
        // A non-empty filter always submits.
        assert!(validate_model("/model gpt-4"));
    }

    #[test]
    fn splits_word_and_args() {
        assert_eq!(split_command("/clear"), ("/clear", ""));
        assert_eq!(split_command("/resume foo"), ("/resume", "foo"));
        assert_eq!(split_command("/model   gpt-4"), ("/model", "gpt-4"));
        assert_eq!(split_command("/rename my chat"), ("/rename", "my chat"));
    }
}
