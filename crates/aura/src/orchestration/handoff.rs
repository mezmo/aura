//! By-reference handoffs between orchestration tasks: whether the coordinator
//! and worker prompts tell agents to pass stored files by name, and which
//! workers the planning context marks as taking them.

use crate::scratchpad::{ByReferenceMap, ScratchpadConfig};

/// Whether any worker with an active scratchpad can reach a tool that takes
/// `<field>_file` references. Each item is one worker's scratchpad override
/// (falling back to `agent_scratchpad`) and the tools it can reach.
///
/// Only then are the handoff instructions worth their tokens: without such a
/// tool, telling the coordinator to forward stored files by name would steer
/// it away from the one path that works (content in the task description).
pub(crate) fn by_reference_handoff_enabled<'a>(
    workers: impl IntoIterator<Item = (Option<&'a ScratchpadConfig>, &'a [String])>,
    agent_scratchpad: Option<&'a ScratchpadConfig>,
    by_reference: &ByReferenceMap,
) -> bool {
    if by_reference.is_empty() {
        return false;
    }
    workers.into_iter().any(|(scratchpad, tools)| {
        scratchpad.or(agent_scratchpad).is_some_and(|sp| sp.enabled)
            && tools.iter().any(|tool| by_reference.contains_tool(tool))
    })
}

/// Planning-context line naming which of a worker's `tools` take stored
/// files by name; empty when none do or the worker's scratchpad is off (the
/// references are only wired up alongside the scratchpad).
pub(crate) fn stored_files_note(
    tools: &[String],
    scratchpad_enabled: bool,
    by_reference: &ByReferenceMap,
) -> String {
    if !scratchpad_enabled {
        return String::new();
    }
    let names: Vec<&str> = tools
        .iter()
        .map(String::as_str)
        .filter(|tool| by_reference.contains_tool(tool))
        .collect();
    if names.is_empty() {
        return String::new();
    }
    format!(
        "\nTakes stored files by name (`<field>_file`) in: {}. Can apply exact edits to \
         stored files (`edit_stored_file`).",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpConfig, McpServerConfig};
    use crate::scratchpad::{FieldPath, by_reference_map};
    use aura_config::ServerScratchpadConfig;
    use std::collections::HashMap;

    /// A map where each of `tools` takes a reference in `content`.
    fn map(tools: &[&str]) -> ByReferenceMap {
        let server = McpServerConfig::HttpStreamable {
            url: "http://test".to_string(),
            headers: HashMap::new(),
            description: None,
            headers_from_request: HashMap::new(),
            scratchpad: ServerScratchpadConfig {
                by_reference: tools
                    .iter()
                    .map(|t| (t.to_string(), vec![FieldPath::parse("content").unwrap()]))
                    .collect(),
                ..Default::default()
            },
        };
        let mcp = McpConfig {
            servers: HashMap::from([("repo".to_string(), server)]),
            sanitize_schemas: false,
        };
        let tools_per_server = HashMap::from([(
            "repo".to_string(),
            vec![
                "get_file".to_string(),
                "write_file".to_string(),
                "list_files".to_string(),
            ],
        )]);
        by_reference_map(Some(&mcp), &tools_per_server)
    }

    fn scratchpad(enabled: bool) -> ScratchpadConfig {
        ScratchpadConfig {
            enabled,
            ..Default::default()
        }
    }

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn enabled_when_a_scratchpad_worker_reaches_a_reference_tool() {
        let on = scratchpad(true);
        let reader = tools(&["get_file"]);
        let writer = tools(&["write_file"]);
        let by_reference = map(&["write_file"]);

        assert!(by_reference_handoff_enabled(
            [(None, reader.as_slice()), (None, writer.as_slice())],
            Some(&on),
            &by_reference,
        ));
    }

    #[test]
    fn disabled_without_a_reachable_reference_tool() {
        let on = scratchpad(true);
        let reader = tools(&["get_file"]);
        assert!(!by_reference_handoff_enabled(
            [(None, reader.as_slice())],
            Some(&on),
            &map(&["write_file"]),
        ));
        assert!(!by_reference_handoff_enabled(
            [(None, reader.as_slice())],
            Some(&on),
            &map(&[]),
        ));
    }

    /// The tool is only wrapped for references when the worker's scratchpad
    /// is on, so a writer with its scratchpad off doesn't count.
    #[test]
    fn worker_scratchpad_override_decides() {
        let on = scratchpad(true);
        let off = scratchpad(false);
        let writer = tools(&["write_file"]);
        let by_reference = map(&["write_file"]);

        assert!(!by_reference_handoff_enabled(
            [(Some(&off), writer.as_slice())],
            Some(&on),
            &by_reference,
        ));
        assert!(by_reference_handoff_enabled(
            [(Some(&on), writer.as_slice())],
            Some(&off),
            &by_reference,
        ));
        assert!(!by_reference_handoff_enabled(
            [(None, writer.as_slice())],
            None,
            &by_reference,
        ));
    }

    #[test]
    fn stored_files_note_names_the_reference_tools() {
        let by_reference = map(&["write_file"]);
        let note = stored_files_note(&tools(&["get_file", "write_file"]), true, &by_reference);
        assert!(note.contains("in: write_file."), "{note}");
        assert!(stored_files_note(&tools(&["get_file"]), true, &by_reference).is_empty());
        assert!(stored_files_note(&tools(&["write_file"]), false, &by_reference).is_empty());
    }
}
