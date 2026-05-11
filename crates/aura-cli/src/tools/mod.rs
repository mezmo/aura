mod definitions;
mod diff;
mod display;
mod execution;
#[cfg(feature = "standalone-cli")]
pub mod rig_tools;

pub use definitions::client_tool_definitions;
pub use diff::*;
pub use display::*;
pub use execution::execute_tool;

/// Check if a tool name is a locally-executed client tool.
pub fn is_local_tool(name: &str) -> bool {
    matches!(
        name,
        "Shell"
            | "Read"
            | "ListFiles"
            | "CompactContext"
            | "Update"
            | "SearchFiles"
            | "FindFiles"
            | "FileInfo"
    )
}

/// Build a tool result message for the case where a server-side tool call has
/// no cached result (no `aura.tool_complete` event arrived for its `tool_call_id`).
///
/// This happens when the server is run without `AURA_CUSTOM_EVENTS=true` (so the
/// client gets the tool call but never the result) or when the event was lost.
/// The message must clearly tell the model the call did NOT succeed, so it does
/// not fabricate output or chain follow-ups on imaginary results.
pub fn missing_server_result_message(tool_name: &str) -> String {
    format!(
        "Error: no result available for server tool '{tool_name}'. \
         The server did not stream tool output (likely missing AURA_CUSTOM_EVENTS=true). \
         Do not assume the call succeeded or fabricate output; tell the user the \
         tool result is unavailable and stop."
    )
}

/// Build a tool result message for a permission denial that guides the LLM
/// toward alternative approaches instead of giving up.
/// `rules_description` is the formatted allow/deny rules from the permission checker.
pub fn permission_denied_message(
    tool_name: &str,
    reason: &str,
    rules_description: Option<&str>,
) -> String {
    let alternatives = match tool_name {
        "Shell" => {
            "The client denied this Shell command. \
            Consider using ListFiles to browse directories or Read to view file contents instead. \
            If you need to run a different command, try a more specific or safer variant."
        }
        "Read" => {
            "The client denied reading this file. \
            Consider using ListFiles to check what files are available, \
            or try reading a different file that may contain the information you need."
        }
        "ListFiles" => {
            "The client denied listing this directory. \
            Consider trying a different directory path, or use Read if you already know the file path."
        }
        "SearchFiles" => {
            "The client denied searching this path. \
            Consider using Read to view specific files, or try searching a different directory."
        }
        "FindFiles" => {
            "The client denied finding files at this path. \
            Consider using ListFiles to browse directories manually, or try a different path."
        }
        "FileInfo" => {
            "The client denied file info for this path. \
            Consider using ListFiles to check what files are available, or Read to view the file."
        }
        "Update" => {
            "The client denied updating this file. \
            Consider using Read to verify the file path and contents first, \
            or try a different file path."
        }
        _ => {
            "The client denied this tool call. \
            Consider using a different tool or approach to accomplish the same goal."
        }
    };
    let rules_section = match rules_description {
        Some(rules) => format!("\n\nClient permission rules:\n{rules}"),
        None => String::new(),
    };
    format!("PERMISSION DENIED: {reason}\n\n{alternatives}{rules_section}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // is_local_tool
    // -----------------------------------------------------------------------

    #[test]
    fn is_local_tool_known_tools() {
        for name in &[
            "Shell",
            "Read",
            "ListFiles",
            "CompactContext",
            "Update",
            "SearchFiles",
            "FindFiles",
            "FileInfo",
        ] {
            assert!(is_local_tool(name), "{name} should be a local tool");
        }
    }

    #[test]
    fn is_local_tool_unknown() {
        assert!(!is_local_tool("UnknownTool"));
        assert!(!is_local_tool("shell")); // case-sensitive
        assert!(!is_local_tool(""));
        assert!(!is_local_tool("vector_search_docs"));
    }

    // -----------------------------------------------------------------------
    // permission_denied_message
    // -----------------------------------------------------------------------

    #[test]
    fn permission_denied_shell() {
        let msg = permission_denied_message("Shell", "blocked", None);
        assert!(msg.contains("PERMISSION DENIED: blocked"));
        assert!(msg.contains("ListFiles"));
        assert!(msg.contains("Read"));
    }

    #[test]
    fn permission_denied_with_rules() {
        let msg = permission_denied_message("Read", "not allowed", Some("allow: Read(*.rs)"));
        assert!(msg.contains("PERMISSION DENIED"));
        assert!(msg.contains("Client permission rules:"));
        assert!(msg.contains("allow: Read(*.rs)"));
    }

    #[test]
    fn permission_denied_unknown_tool() {
        let msg = permission_denied_message("FakeTool", "nope", None);
        assert!(msg.contains("different tool or approach"));
    }
}
