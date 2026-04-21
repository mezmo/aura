use crossterm::style::Stylize;

/// Format a tool call for display: "Shell(ls -la)" or "Read(/path/to/file, offset=500)"
pub fn format_tool_call_display(name: &str, arguments: &str) -> String {
    let args: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::Value::String(arguments.to_string()));
    match name {
        "Shell" => {
            let cmd = args["command"].as_str().unwrap_or(arguments);
            format!("Shell({cmd})")
        }
        "Read" => {
            let path = args["file_path"].as_str().unwrap_or("?");
            let offset = args["offset"].as_u64().unwrap_or(0);
            if offset > 0 {
                let limit = args["limit"].as_u64().unwrap_or(500);
                format!("Read({path}, offset={offset}, limit={limit})")
            } else {
                format!("Read({path})")
            }
        }
        "ListFiles" => {
            let path = args["path"].as_str().unwrap_or("?");
            format!("ListFiles({path})")
        }
        "SearchFiles" => {
            let pattern = args["pattern"].as_str().unwrap_or("?");
            let path = args["path"].as_str().unwrap_or(".");
            format!("SearchFiles(\"{pattern}\", {path})")
        }
        "FindFiles" => {
            let pattern = args["pattern"].as_str().unwrap_or("?");
            let path = args["path"].as_str().unwrap_or(".");
            format!("FindFiles(\"{pattern}\", {path})")
        }
        "FileInfo" => {
            let path = args["path"].as_str().unwrap_or("?");
            format!("FileInfo({path})")
        }
        "CompactContext" => "CompactContext()".to_string(),
        "Update" => {
            let path = args["file_path"].as_str().unwrap_or("?");
            format!("Update({path})")
        }
        _ => format!("{name}({arguments})"),
    }
}

/// Format a tool call for display from a BTreeMap of arguments.
/// Converts args to JSON and delegates to `format_tool_call_display`.
pub fn format_tool_call_display_from_args(
    name: &str,
    args: &std::collections::BTreeMap<String, serde_json::Value>,
) -> String {
    let map: serde_json::Map<String, serde_json::Value> =
        args.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let json_str = serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default();
    format_tool_call_display(name, &json_str)
}

/// Extract the command name from a Shell arguments JSON string.
/// e.g., {"command": "sed -i '' 's/foo/bar/' file.txt"} -> "sed"
pub fn extract_command_name(arguments: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let command = args["command"].as_str().unwrap_or("");
    let trimmed = command.trim();
    // Handle env prefixes like "VAR=val cmd ..."
    // and common prefixes like "sudo cmd ..."
    trimmed
        .split_whitespace()
        .find(|token| !token.contains('='))
        .unwrap_or("")
        .to_string()
}

/// Extract a short display name for a tool call (e.g. the file path or command token).
pub fn extract_tool_display_name(tool_name: &str, arguments: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    match tool_name {
        "Shell" => extract_command_name(arguments),
        "Read" => args["file_path"].as_str().unwrap_or("").to_string(),
        "ListFiles" | "FindFiles" | "SearchFiles" | "FileInfo" => {
            args["path"].as_str().unwrap_or("").to_string()
        }
        "Update" => args["file_path"].as_str().unwrap_or("").to_string(),
        "CompactContext" => String::new(),
        _ => tool_name.to_string(),
    }
}

/// Return a past-tense summary header for a group of tool calls.
pub fn format_tool_group_header(tool_name: &str, count: usize) -> String {
    match tool_name {
        "Read" => format!(
            "Read {} {}",
            count,
            if count == 1 { "file" } else { "files" }
        ),
        "Shell" => format!(
            "Ran {} {}",
            count,
            if count == 1 { "command" } else { "commands" }
        ),
        "ListFiles" => format!(
            "Listed {} {}",
            count,
            if count == 1 {
                "directory"
            } else {
                "directories"
            }
        ),
        "SearchFiles" => format!(
            "Searched {} {}",
            count,
            if count == 1 { "path" } else { "paths" }
        ),
        "FindFiles" => format!(
            "Found files in {} {}",
            count,
            if count == 1 { "path" } else { "paths" }
        ),
        "FileInfo" => format!(
            "Inspected {} {}",
            count,
            if count == 1 { "path" } else { "paths" }
        ),
        "Update" => format!(
            "Updated {} {}",
            count,
            if count == 1 { "file" } else { "files" }
        ),
        "CompactContext" => "Compacted context".to_string(),
        _ => format!(
            "{}: {} {}",
            crate::api::types::snake_to_pascal_case(tool_name),
            count,
            if count == 1 { "call" } else { "calls" }
        ),
    }
}

/// Print a grouped tool summary with a colored bullet header and optional items line.
pub fn print_tool_group(header: &str, display_names: &[String], expanded: bool) {
    let color = crate::ui::prompt::random_bullet_color();
    println!(
        "{} {}",
        "●".with(color).attribute(crossterm::style::Attribute::Bold),
        header.with(crossterm::style::Color::White),
    );

    if !display_names.is_empty() {
        // Deduplicate while preserving order
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<&str> = display_names
            .iter()
            .filter(|n| !n.is_empty() && seen.insert(n.as_str()))
            .map(|n| n.as_str())
            .collect();

        if !deduped.is_empty() {
            let connector = if expanded { "└─" } else { "├─" };
            println!(
                "{} {}",
                connector.with(crossterm::style::Color::DarkGrey),
                deduped.join(", ").with(crossterm::style::Color::DarkGrey),
            );
        }
    }

    if !expanded {
        let connector = "└─";
        println!(
            "{} {}",
            connector.with(crossterm::style::Color::DarkGrey),
            "... /expand to see more".with(crossterm::style::Color::DarkGrey),
        );
    }
}

/// Print a tool call with args displayed as a key/value tree.
///
/// Compact (`max_keys` > 0):
/// ```text
/// ● ToolName
/// ├─ key1
/// │  └─ value1
/// └─ ... +N more (/expand)
/// ```
///
/// Expanded (`max_keys` == 0 means show all):
/// ```text
/// ● ToolName
/// ├─ key1
/// │  └─ value1
/// └─ key2
///    └─ value2
/// ```
pub fn print_tool_call_tree(
    name: &str,
    args: &std::collections::BTreeMap<String, serde_json::Value>,
    max_keys: usize,
) {
    use crossterm::style::{Attribute, Color, Stylize};

    let bullet_color = crate::ui::prompt::random_bullet_color();
    let key_color = Color::Rgb {
        r: 100,
        g: 149,
        b: 237,
    }; // Cornflower blue

    let display_name = crate::api::types::snake_to_pascal_case(name);
    println!(
        "{} {}",
        "●".with(bullet_color).attribute(Attribute::Bold),
        display_name.as_str().with(Color::White),
    );

    let keys: Vec<(&String, &serde_json::Value)> = args.iter().collect();
    let total = keys.len();
    let show_count = if max_keys > 0 && total > max_keys {
        max_keys
    } else {
        total
    };

    let has_overflow = max_keys > 0 && total > max_keys;

    for (idx, (key, value)) in keys[..show_count].iter().enumerate() {
        let is_last = idx == show_count - 1 && !has_overflow;
        let connector = if is_last { "└─" } else { "├─" };
        let child_cont = if is_last { "   " } else { "│  " };
        println!(
            "{} {}",
            connector.with(Color::DarkGrey),
            key.as_str().with(key_color),
        );
        let val_str = format_arg_value(value);
        let val_lines: Vec<&str> = val_str.lines().collect();
        let val_total = val_lines.len();
        for (vi, line) in val_lines.iter().enumerate() {
            let val_connector = if vi == val_total - 1 {
                "└─"
            } else {
                "├─"
            };
            println!(
                "{}{} {}",
                child_cont.with(Color::DarkGrey),
                val_connector.with(Color::DarkGrey),
                line.with(Color::DarkGrey),
            );
        }
    }

    if has_overflow {
        let remaining = total - max_keys;
        println!(
            "{} {}",
            "└─".with(Color::DarkGrey),
            format!("... +{remaining} more (/expand)").with(Color::DarkGrey),
        );
    }
}

/// Format a JSON value for display as a tree value.
/// Strings are shown without quotes; other types use compact JSON.
fn format_arg_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// Format up to 3 args as `key: "val", key: 50`-style string for a tool label.
/// Filters out keys starting with `_` and null/empty/"null" values; truncates
/// string values longer than 20 chars to `"<17 chars>..."`. Used to mirror the
/// orchestrator-style tool-call summary in single-agent rendering.
pub fn format_args_summary(args: &std::collections::BTreeMap<String, serde_json::Value>) -> String {
    args.iter()
        .filter(|(k, v)| {
            !k.starts_with('_')
                && !matches!(v, serde_json::Value::Null)
                && !matches!(v, serde_json::Value::String(s) if s.is_empty() || s == "null")
        })
        .take(3)
        .map(|(k, v)| {
            let val_str = match v {
                serde_json::Value::String(s) => {
                    if s.chars().count() > 20 {
                        let prefix: String = s.chars().take(17).collect();
                        format!("\"{prefix}...\"")
                    } else {
                        format!("\"{s}\"")
                    }
                }
                other => {
                    let s = other.to_string();
                    if s.chars().count() > 20 {
                        let prefix: String = s.chars().take(17).collect();
                        format!("{prefix}...")
                    } else {
                        s
                    }
                }
            };
            format!("{k}: {val_str}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a tool-call label like `Head(file: "task_1...", lines: 60)` —
/// PascalCase tool name plus an `format_args_summary` parenthesised arg list.
pub fn format_tool_call_label(
    tool_name: &str,
    args: &std::collections::BTreeMap<String, serde_json::Value>,
) -> String {
    let display_name = crate::api::types::snake_to_pascal_case(tool_name);
    let summary = format_args_summary(args);
    format!("{display_name}({summary})")
}

/// Normalize a tool result string for display. Handles two cases where the
/// result reaches us pre-encoded:
///
/// 1. The whole text is a JSON-encoded string (wrapped in `"…"` with escape
///    sequences inside). Parsed via `serde_json` to recover the original
///    multi-line text.
/// 2. The text is a single line containing literal `\n`/`\t` escape
///    sequences (no real newlines). Decoded inline.
///
/// Returns the input unchanged when it already looks like raw multi-line
/// text — this avoids touching content that legitimately includes a
/// backslash-n sequence in a normal multi-line result.
pub fn normalize_tool_result_text(text: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = text.trim();
    if trimmed.starts_with('"')
        && trimmed.ends_with('"')
        && trimmed.len() >= 2
        && let Ok(decoded) = serde_json::from_str::<String>(trimmed)
    {
        return std::borrow::Cow::Owned(decoded);
    }

    if text.contains('\n') || !text.contains("\\n") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // format_tool_call_display
    // -----------------------------------------------------------------------

    #[test]
    fn format_display_shell() {
        let result = format_tool_call_display("Shell", r#"{"command":"ls -la"}"#);
        assert_eq!(result, "Shell(ls -la)");
    }

    #[test]
    fn format_display_read_no_offset() {
        let result = format_tool_call_display("Read", r#"{"file_path":"/tmp/foo.rs"}"#);
        assert_eq!(result, "Read(/tmp/foo.rs)");
    }

    #[test]
    fn format_display_read_with_offset() {
        let result = format_tool_call_display(
            "Read",
            r#"{"file_path":"/tmp/foo.rs","offset":100,"limit":50}"#,
        );
        assert_eq!(result, "Read(/tmp/foo.rs, offset=100, limit=50)");
    }

    #[test]
    fn format_display_list_files() {
        let result = format_tool_call_display("ListFiles", r#"{"path":"/tmp"}"#);
        assert_eq!(result, "ListFiles(/tmp)");
    }

    #[test]
    fn format_display_search_files() {
        let result = format_tool_call_display("SearchFiles", r#"{"pattern":"TODO","path":"src"}"#);
        assert_eq!(result, r#"SearchFiles("TODO", src)"#);
    }

    #[test]
    fn format_display_find_files() {
        let result = format_tool_call_display("FindFiles", r#"{"pattern":"*.rs","path":"src"}"#);
        assert_eq!(result, r#"FindFiles("*.rs", src)"#);
    }

    #[test]
    fn format_display_file_info() {
        let result = format_tool_call_display("FileInfo", r#"{"path":"/tmp/foo"}"#);
        assert_eq!(result, "FileInfo(/tmp/foo)");
    }

    #[test]
    fn format_display_compact_context() {
        let result = format_tool_call_display("CompactContext", "{}");
        assert_eq!(result, "CompactContext()");
    }

    #[test]
    fn format_display_update() {
        let result = format_tool_call_display("Update", r#"{"file_path":"src/main.rs"}"#);
        assert_eq!(result, "Update(src/main.rs)");
    }

    #[test]
    fn format_display_unknown_tool() {
        let result = format_tool_call_display("CustomTool", r#"{"key":"val"}"#);
        assert_eq!(result, r#"CustomTool({"key":"val"})"#);
    }

    #[test]
    fn format_display_invalid_json_falls_back() {
        let result = format_tool_call_display("Shell", "not json");
        assert_eq!(result, "Shell(not json)");
    }

    // -----------------------------------------------------------------------
    // format_tool_call_display_from_args
    // -----------------------------------------------------------------------

    #[test]
    fn format_display_from_args_shell() {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("echo hello".to_string()),
        );
        let result = format_tool_call_display_from_args("Shell", &args);
        assert_eq!(result, "Shell(echo hello)");
    }

    // -----------------------------------------------------------------------
    // extract_command_name
    // -----------------------------------------------------------------------

    #[test]
    fn extract_command_name_simple() {
        assert_eq!(extract_command_name(r#"{"command":"ls -la"}"#), "ls");
    }

    #[test]
    fn extract_command_name_with_env() {
        assert_eq!(
            extract_command_name(r#"{"command":"VAR=val FOO=bar cmd arg"}"#),
            "cmd"
        );
    }

    #[test]
    fn extract_command_name_sudo() {
        // sudo contains no '=', so it's returned as the command name
        assert_eq!(
            extract_command_name(r#"{"command":"sudo rm -rf /"}"#),
            "sudo"
        );
    }

    #[test]
    fn extract_command_name_empty() {
        assert_eq!(extract_command_name(r#"{"command":""}"#), "");
    }

    #[test]
    fn extract_command_name_invalid_json() {
        assert_eq!(extract_command_name("not json"), "");
    }

    // -----------------------------------------------------------------------
    // extract_tool_display_name
    // -----------------------------------------------------------------------

    #[test]
    fn extract_display_name_shell() {
        let result = extract_tool_display_name("Shell", r#"{"command":"git status"}"#);
        assert_eq!(result, "git");
    }

    #[test]
    fn extract_display_name_read() {
        let result = extract_tool_display_name("Read", r#"{"file_path":"src/main.rs"}"#);
        assert_eq!(result, "src/main.rs");
    }

    #[test]
    fn extract_display_name_list_files() {
        let result = extract_tool_display_name("ListFiles", r#"{"path":"/tmp"}"#);
        assert_eq!(result, "/tmp");
    }

    #[test]
    fn extract_display_name_compact_context() {
        let result = extract_tool_display_name("CompactContext", "{}");
        assert_eq!(result, "");
    }

    #[test]
    fn extract_display_name_unknown_tool() {
        let result = extract_tool_display_name("CustomTool", "{}");
        assert_eq!(result, "CustomTool");
    }

    // -----------------------------------------------------------------------
    // format_tool_group_header
    // -----------------------------------------------------------------------

    #[test]
    fn group_header_read_singular() {
        assert_eq!(format_tool_group_header("Read", 1), "Read 1 file");
    }

    #[test]
    fn group_header_read_plural() {
        assert_eq!(format_tool_group_header("Read", 3), "Read 3 files");
    }

    #[test]
    fn group_header_shell() {
        assert_eq!(format_tool_group_header("Shell", 1), "Ran 1 command");
        assert_eq!(format_tool_group_header("Shell", 5), "Ran 5 commands");
    }

    #[test]
    fn group_header_list_files() {
        assert_eq!(
            format_tool_group_header("ListFiles", 1),
            "Listed 1 directory"
        );
        assert_eq!(
            format_tool_group_header("ListFiles", 2),
            "Listed 2 directories"
        );
    }

    #[test]
    fn group_header_update() {
        assert_eq!(format_tool_group_header("Update", 1), "Updated 1 file");
    }

    #[test]
    fn group_header_compact_context() {
        assert_eq!(
            format_tool_group_header("CompactContext", 1),
            "Compacted context"
        );
    }

    #[test]
    fn group_header_unknown_tool() {
        let result = format_tool_group_header("custom_tool", 2);
        assert!(result.contains("CustomTool")); // snake_to_pascal_case
        assert!(result.contains("2 calls"));
    }

    // -----------------------------------------------------------------------
    // format_arg_value
    // -----------------------------------------------------------------------

    #[test]
    fn format_arg_value_string() {
        let val = serde_json::Value::String("hello".to_string());
        assert_eq!(format_arg_value(&val), "hello");
    }

    #[test]
    fn format_arg_value_null() {
        assert_eq!(format_arg_value(&serde_json::Value::Null), "null");
    }

    #[test]
    fn format_arg_value_bool() {
        assert_eq!(format_arg_value(&serde_json::Value::Bool(true)), "true");
    }

    #[test]
    fn format_arg_value_number() {
        let val = serde_json::json!(42);
        assert_eq!(format_arg_value(&val), "42");
    }

    #[test]
    fn format_arg_value_array() {
        let val = serde_json::json!([1, 2, 3]);
        assert_eq!(format_arg_value(&val), "[1,2,3]");
    }
}
