use serde_json::json;

use crate::api::types::{FunctionDefinition, ToolDefinition};

/// Returns the tool definitions to send to the API.
pub fn client_tool_definitions() -> Vec<ToolDefinition> {
    let (shell_platform_note, shell_command_note) = if cfg!(windows) {
        (
            "Commands run on Windows via cmd.exe /C — use Windows cmd syntax \
             (%VAR% for env-var expansion, not $VAR; no single-quote string literals; \
             builtins like dir/copy/move rather than ls/cp/mv).",
            "The shell command to execute (passed to cmd.exe /C — Windows cmd syntax)",
        )
    } else {
        (
            "Commands run via sh -c — use POSIX shell syntax \
             (pipes, &&, $VAR expansion, redirections like 2>&1).",
            "The shell command to execute (passed to sh -c — POSIX shell syntax)",
        )
    };

    vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "Shell".to_string(),
                description: format!(
                    "LAST RESORT — only use Shell when no other tool can accomplish the task. \
                    NEVER use Shell for listing files/directories (use ListFiles/FindFiles), \
                    reading file contents (use Read), searching file contents (use SearchFiles), \
                    or getting file metadata (use FileInfo). \
                    Shell is ONLY for operations like running builds, git commands, \
                    package managers, or other commands that have no dedicated tool equivalent. \
                    {shell_platform_note}"
                ),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": shell_command_note
                        }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "Read".to_string(),
                description: "Read a file from the user's local machine with chunked streaming. \
                    Returns lines with line numbers and metadata (total_lines, has_more, next_offset) \
                    so you know if you need to call again with an offset to read more. \
                    Use offset and limit to read large files in chunks. \
                    Use ListFiles first to discover files, then Read to view their contents."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Absolute or relative path to the file to read"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "0-based line offset to start reading from (default: 0)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to return (default: 500)"
                        }
                    },
                    "required": ["file_path"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "ListFiles".to_string(),
                description: "Assume the 'starting directory' to be the current working directory if not specified. \
                    List files and directories at a given path on the user's local machine. \
                    Returns entries with type (file/dir/symlink), size in bytes, and name. \
                    Use this to explore the filesystem before reading files with Read. \
                    Only lists the immediate contents of a single directory. \
                    To explore subdirectories, call ListFiles again on each subdirectory of interest."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or relative path to the directory to list"
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "Update".to_string(),
                description: "Signal intent to update or create a file. After calling Update, \
                    use one or more Shell calls to make the actual modifications (sed, python, \
                    cat, tee, etc.). The Shell calls following an Update are automatically \
                    approved and grouped under the Update display. Call Update once per file \
                    you intend to modify."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Absolute or relative path to the file to update or create"
                        }
                    },
                    "required": ["file_path"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "SearchFiles".to_string(),
                description: "Search file contents for a pattern (like grep). Returns matching lines \
                    with file path, line number, and optional context lines. Skips hidden directories \
                    (.git, node_modules, target, etc.) and binary files. Use this instead of \
                    Shell(grep ...) for searching code."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "The search pattern (literal string or regex)"
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory or file path to search in"
                        },
                        "regex": {
                            "type": "boolean",
                            "description": "Treat pattern as a regex (default: false, literal match)"
                        },
                        "case_sensitive": {
                            "type": "boolean",
                            "description": "Case-sensitive matching (default: true)"
                        },
                        "lines_before": {
                            "type": "integer",
                            "description": "Number of context lines before each match (like grep -B)"
                        },
                        "lines_after": {
                            "type": "integer",
                            "description": "Number of context lines after each match (like grep -A)"
                        },
                        "include_pattern": {
                            "type": "string",
                            "description": "Glob pattern to filter filenames (e.g. \"*.rs\", \"*.py\")"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of matching lines to return (default: 100)"
                        }
                    },
                    "required": ["pattern", "path"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "FindFiles".to_string(),
                description: "Recursively find files and directories matching a glob pattern. \
                    Returns results with type, size, and path — same format as ListFiles. \
                    Skips hidden directories (.git, node_modules, target, etc.). \
                    Use this instead of Shell(find ...) for locating files."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Root directory to search from"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match file/directory names (e.g. \"*.rs\", \"Cargo.*\")"
                        },
                        "type_filter": {
                            "type": "string",
                            "enum": ["file", "dir", "any"],
                            "description": "Filter by entry type (default: \"any\")"
                        },
                        "max_depth": {
                            "type": "integer",
                            "description": "Maximum directory depth to recurse (default: unlimited)"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of results to return (default: 200)"
                        }
                    },
                    "required": ["path", "pattern"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "FileInfo".to_string(),
                description: "Get metadata about a file or directory without reading its contents. \
                    Returns line count, byte size, type, last modified time, and permissions. \
                    For directories: shows entry count breakdown (files vs dirs). \
                    Use this to decide whether to Read the whole file or use SearchFiles/offset."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or relative path to the file or directory"
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "CompactContext".to_string(),
                description: "Compact the conversation context by discarding the first half of \
                    the chat history while preserving the system prompt. Use this when the \
                    conversation is very long and context is running low, or when the user \
                    requests compaction. This is a destructive operation — discarded messages \
                    cannot be recovered."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_tool_definitions_has_expected_tools() {
        let defs = client_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"Shell"));
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"ListFiles"));
        assert!(names.contains(&"Update"));
        assert!(names.contains(&"SearchFiles"));
        assert!(names.contains(&"FindFiles"));
        assert!(names.contains(&"FileInfo"));
        assert!(names.contains(&"CompactContext"));
        assert_eq!(defs.len(), 8);
    }
}
