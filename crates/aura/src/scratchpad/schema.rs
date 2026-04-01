//! Structure analysis with line ranges for JSON and Markdown.
//!
//! For JSON: shows keys, types, array lengths, and line ranges.
//! For Markdown: shows section hierarchy, keys, and line ranges.

use serde_json::Value;

/// A node in the JSON structure tree.
#[derive(Debug, Clone)]
pub struct SchemaNode {
    pub path: String,
    pub kind: String,
    pub line_start: usize,
    pub line_end: usize,
    pub children: Vec<SchemaNode>,
}

/// Analyze JSON content and produce a structure tree with line ranges.
///
/// The content should be the raw JSON string (used for line counting).
/// Returns None if the content is not valid JSON.
pub fn analyze_json_structure(content: &str) -> Option<SchemaNode> {
    let value: Value = serde_json::from_str(content).ok()?;

    // Build a line-offset index for mapping byte offsets to lines.
    let line_offsets = build_line_offsets(content);

    // We'll walk the pretty-printed JSON for line mapping since raw JSON
    // may be compact. For the POC, we use the original content's lines.
    let root = build_node("$", &value, content, &line_offsets, 0);
    Some(root)
}

/// Format a schema tree into a human-readable string with line numbers.
pub fn format_schema(node: &SchemaNode, max_depth: usize) -> String {
    let mut output = String::new();
    format_node(&mut output, node, 0, max_depth);
    output
}

fn format_node(output: &mut String, node: &SchemaNode, depth: usize, max_depth: usize) {
    if depth > max_depth {
        return;
    }
    let indent = "  ".repeat(depth);
    output.push_str(&format!(
        "{}{} ({}) [L{}-L{}]\n",
        indent, node.path, node.kind, node.line_start, node.line_end
    ));
    for child in &node.children {
        format_node(output, child, depth + 1, max_depth);
    }
}

fn build_line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

fn byte_offset_to_line(offsets: &[usize], offset: usize) -> usize {
    match offsets.binary_search(&offset) {
        Ok(line) => line + 1,
        Err(line) => line, // line is the insertion point = 1-indexed line
    }
}

/// Find the byte offset of a substring within content, starting from `search_from`.
fn find_key_offset(content: &str, key: &str, search_from: usize) -> Option<usize> {
    let needle = format!("\"{}\"", key);
    content[search_from..]
        .find(&needle)
        .map(|i| i + search_from)
}

fn build_node(
    path: &str,
    value: &Value,
    content: &str,
    line_offsets: &[usize],
    search_from: usize,
) -> SchemaNode {
    match value {
        Value::Object(map) => {
            // Find where this object starts in content
            let obj_start = content[search_from..]
                .find('{')
                .map(|i| i + search_from)
                .unwrap_or(search_from);
            let mut children = Vec::new();
            let mut cursor = obj_start;

            for (key, val) in map {
                let key_offset = find_key_offset(content, key, cursor).unwrap_or(cursor);
                let child_path = if path == "$" {
                    format!("$.{}", key)
                } else {
                    format!("{}.{}", path, key)
                };
                let child = build_node(&child_path, val, content, line_offsets, key_offset);
                cursor = key_offset + 1;
                children.push(child);
            }

            // Find matching close brace
            let obj_end = find_matching_brace(content, obj_start).unwrap_or(content.len() - 1);
            let line_start = byte_offset_to_line(line_offsets, obj_start);
            let line_end = byte_offset_to_line(line_offsets, obj_end);

            SchemaNode {
                path: path.to_string(),
                kind: format!("object({} keys)", map.len()),
                line_start,
                line_end,
                children,
            }
        }
        Value::Array(arr) => {
            let arr_start = content[search_from..]
                .find('[')
                .map(|i| i + search_from)
                .unwrap_or(search_from);

            let mut children = Vec::new();
            if !arr.is_empty() {
                // Show schema of first element as representative
                let child_path = format!("{}[0]", path);
                let child = build_node(&child_path, &arr[0], content, line_offsets, arr_start + 1);
                children.push(child);
            }

            let arr_end = find_matching_bracket(content, arr_start).unwrap_or(content.len() - 1);
            let line_start = byte_offset_to_line(line_offsets, arr_start);
            let line_end = byte_offset_to_line(line_offsets, arr_end);

            SchemaNode {
                path: path.to_string(),
                kind: format!("array({} items)", arr.len()),
                line_start,
                line_end,
                children,
            }
        }
        Value::String(s) => {
            let line = byte_offset_to_line(line_offsets, search_from);
            let preview = if s.len() > 50 {
                format!("string({}ch)", s.len())
            } else {
                "string".to_string()
            };
            SchemaNode {
                path: path.to_string(),
                kind: preview,
                line_start: line,
                line_end: line,
                children: vec![],
            }
        }
        Value::Number(_) => {
            let line = byte_offset_to_line(line_offsets, search_from);
            SchemaNode {
                path: path.to_string(),
                kind: "number".to_string(),
                line_start: line,
                line_end: line,
                children: vec![],
            }
        }
        Value::Bool(_) => {
            let line = byte_offset_to_line(line_offsets, search_from);
            SchemaNode {
                path: path.to_string(),
                kind: "bool".to_string(),
                line_start: line,
                line_end: line,
                children: vec![],
            }
        }
        Value::Null => {
            let line = byte_offset_to_line(line_offsets, search_from);
            SchemaNode {
                path: path.to_string(),
                kind: "null".to_string(),
                line_start: line,
                line_end: line,
                children: vec![],
            }
        }
    }
}

fn find_matching_brace(content: &str, open_pos: usize) -> Option<usize> {
    find_matching_delimiter(content, open_pos, b'{', b'}')
}

fn find_matching_bracket(content: &str, open_pos: usize) -> Option<usize> {
    find_matching_delimiter(content, open_pos, b'[', b']')
}

fn find_matching_delimiter(content: &str, open_pos: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, &byte) in bytes.iter().enumerate().skip(open_pos) {
        if escape {
            escape = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escape = true,
            b'"' => in_string = !in_string,
            b if b == open && !in_string => depth += 1,
            b if b == close && !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

// ============================================================================
// Markdown structure analysis
// ============================================================================

/// A section in a markdown document, identified by a `#` header.
#[derive(Debug, Clone)]
pub struct MarkdownSection {
    /// The header text (without the `#` prefix).
    pub title: String,
    /// Header depth (1 for `#`, 2 for `##`, 3 for `###`, etc.).
    pub depth: usize,
    /// 1-indexed start line of the header.
    pub line_start: usize,
    /// 1-indexed end line (last line before the next section or EOF).
    pub line_end: usize,
    /// Top-level list keys found in this section (e.g., "version", "total_groups").
    pub keys: Vec<String>,
    /// Child sections (nested headers of greater depth).
    pub children: Vec<MarkdownSection>,
}

/// Analyze markdown content and produce a section tree with line ranges and keys.
///
/// Returns None if the content has no markdown headers.
pub fn analyze_markdown_structure(content: &str) -> Option<Vec<MarkdownSection>> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // First pass: find all headers with their positions and depths
    let mut headers: Vec<(usize, usize, String)> = Vec::new(); // (line_idx, depth, title)
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            let depth = trimmed.chars().take_while(|&c| c == '#').count();
            let title = trimmed[depth..].trim().to_string();
            if !title.is_empty() {
                headers.push((i, depth, title));
            }
        }
    }

    if headers.is_empty() {
        return None;
    }

    // Build flat sections with line ranges and keys
    let total_lines = lines.len();
    let mut flat_sections: Vec<MarkdownSection> = Vec::new();

    for (idx, (line_idx, depth, title)) in headers.iter().enumerate() {
        let line_start = line_idx + 1; // 1-indexed
        let line_end = if idx + 1 < headers.len() {
            headers[idx + 1].0 // line before next header (0-indexed)
        } else {
            total_lines // EOF
        };

        // Collect top-level list keys in this section
        let mut keys = Vec::new();
        for line in &lines[*line_idx + 1..line_end] {
            let trimmed = line.trim_start();
            // Top-level list items (not indented sub-items)
            if trimmed.starts_with("- ")
                && !line.starts_with("  ")
                && let Some(key) = trimmed.strip_prefix("- ")
            {
                // Extract key from "key: value", "key:", or bare "key" patterns
                let key_str = if let Some((k, _)) = key.split_once(": ") {
                    k.to_string()
                } else {
                    key.trim_end_matches(':').to_string()
                };
                keys.push(key_str);
            }
        }

        flat_sections.push(MarkdownSection {
            title: title.clone(),
            depth: *depth,
            line_start,
            line_end,
            keys,
            children: Vec::new(),
        });
    }

    // Build tree structure by nesting children under parents
    let sections = build_section_tree(&flat_sections);
    Some(sections)
}

/// Build a tree of sections from a flat list, nesting deeper sections under shallower ones.
fn build_section_tree(flat: &[MarkdownSection]) -> Vec<MarkdownSection> {
    let mut result: Vec<MarkdownSection> = Vec::new();
    let mut stack: Vec<MarkdownSection> = Vec::new();

    for section in flat {
        // Pop sections from the stack that are at the same or deeper depth
        while let Some(top) = stack.last() {
            if top.depth >= section.depth {
                let completed = stack.pop().unwrap();
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(completed);
                } else {
                    result.push(completed);
                }
            } else {
                break;
            }
        }
        stack.push(section.clone());
    }

    // Flush remaining stack
    while let Some(completed) = stack.pop() {
        if let Some(parent) = stack.last_mut() {
            parent.children.push(completed);
        } else {
            result.push(completed);
        }
    }

    result
}

/// Format a markdown section tree into a human-readable string with line numbers.
pub fn format_markdown_schema(sections: &[MarkdownSection], max_depth: usize) -> String {
    let mut output = String::new();
    for section in sections {
        format_md_section(&mut output, section, 0, max_depth);
    }
    output
}

fn format_md_section(
    output: &mut String,
    section: &MarkdownSection,
    indent: usize,
    max_depth: usize,
) {
    if indent > max_depth {
        return;
    }
    let prefix = "  ".repeat(indent);
    output.push_str(&format!(
        "{}{} {} [L{}-L{}]",
        prefix,
        "#".repeat(section.depth),
        section.title,
        section.line_start,
        section.line_end,
    ));

    if !section.keys.is_empty() {
        output.push_str(&format!(" (keys: {})", section.keys.join(", ")));
    }
    output.push('\n');

    for child in &section.children {
        format_md_section(output, child, indent + 1, max_depth);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_object() {
        let json = r#"{
  "name": "test",
  "count": 42,
  "active": true
}"#;
        let node = analyze_json_structure(json).unwrap();
        assert_eq!(node.path, "$");
        assert!(node.kind.contains("3 keys"));
        assert_eq!(node.children.len(), 3);
    }

    #[test]
    fn test_nested_array() {
        let json = r#"{
  "items": [
    {"id": 1, "value": "a"},
    {"id": 2, "value": "b"}
  ]
}"#;
        let node = analyze_json_structure(json).unwrap();
        assert_eq!(node.children.len(), 1);
        let items = &node.children[0];
        assert!(items.kind.contains("2 items"));
        // Should show first element as representative
        assert_eq!(items.children.len(), 1);
    }

    #[test]
    fn test_format_schema() {
        let json = r#"{"a": 1, "b": {"c": "hello"}}"#;
        let node = analyze_json_structure(json).unwrap();
        let formatted = format_schema(&node, 10);
        assert!(formatted.contains("$.a"));
        assert!(formatted.contains("$.b"));
        assert!(formatted.contains("$.b.c"));
    }

    #[test]
    fn test_invalid_json() {
        assert!(analyze_json_structure("not json").is_none());
    }

    #[test]
    fn test_markdown_sections() {
        let md = "\
### Root Cause Analysis
- version: v2-doc
- window_ms: 1234..=5678

### Summary
- total_groups: 2
- total_logs_weighted: 15
- by_level:
  - ERROR: 5
  - WARN: 10

### Groups
- group: 1
  - id: abc
  - level: WARN
- group: 2
  - id: def
  - level: ERROR";

        let sections = analyze_markdown_structure(md).unwrap();
        assert_eq!(sections.len(), 3);

        assert_eq!(sections[0].title, "Root Cause Analysis");
        assert_eq!(sections[0].line_start, 1);
        assert_eq!(sections[0].line_end, 4); // before Summary header
        assert_eq!(sections[0].keys, vec!["version", "window_ms"]);

        assert_eq!(sections[1].title, "Summary");
        assert_eq!(sections[1].line_start, 5);
        assert_eq!(sections[1].line_end, 11); // before Groups header
        assert_eq!(
            sections[1].keys,
            vec!["total_groups", "total_logs_weighted", "by_level"]
        );

        assert_eq!(sections[2].title, "Groups");
        assert_eq!(sections[2].line_start, 12);
        assert_eq!(sections[2].line_end, 18); // EOF
        assert_eq!(sections[2].keys, vec!["group", "group"]);
    }

    #[test]
    fn test_markdown_format_output() {
        let md = "### Header\n- key1: val\n- key2: val\n";
        let sections = analyze_markdown_structure(md).unwrap();
        let formatted = format_markdown_schema(&sections, 10);
        assert!(formatted.contains("### Header"));
        assert!(formatted.contains("L1-L3"));
        assert!(formatted.contains("keys: key1, key2"));
    }

    #[test]
    fn test_markdown_no_headers() {
        assert!(analyze_markdown_structure("just plain text\nwith lines").is_none());
    }

    #[test]
    fn test_markdown_mezmo_rca_format() {
        // Realistic Mezmo analyze_logs kv_markdown output
        let md = "\
### Root Cause Analysis
- version: v2-doc
- window_ms: 1775078646300..=1775078946300
- earliest_observed_ts_ms: 1775078919140

### Summary
- total_groups: 2
- total_logs_weighted: 15
- by_level:
  - ERROR: 5
  - WARN: 10
- top_apps:
  - prometheus-server: 15

### Groups
- group: 1
  - id: fb4f3269219fcffc
  - level: WARN
  - app: prometheus-server
  - host: prometheus-78c65d57fd-jfqvj
  - count: 10
  - percent_of_total: 1.00
  - first_ts_ms: 1775078919140
  - last_ts_ms: 1775078919140
  - first_rel_ms: 0
  - last_rel_ms: 0
  - template: `time=[VAR]-[VAR]-[VAR]T[VAR]:[VAR]:[VAR]Z level=WARN source=write_handler.go:[VAR] msg=\"Error on ingesting out-of-order exemplars\" component=web num_dropped=[VAR]`
  - representatives:
    - 1:
      - ts_ms: 1775078919140
      - message: `time=2026-04-01T21:28:39.140Z level=WARN source=write_handler.go:288 msg=\"Error on ingesting out-of-order exemplars\" component=web num_dropped=225`
- group: 2
  - id: 4294cace96cb35a3
  - level: ERROR
  - app: prometheus-server
  - host: prometheus-78c65d57fd-jfqvj
  - count: 5
  - percent_of_total: 1.00
  - first_ts_ms: 1775078919540
  - last_ts_ms: 1775078919540
  - first_rel_ms: 400
  - last_rel_ms: 400
  - template: `time=[VAR]-[VAR]-[VAR]T[VAR]:[VAR]:[VAR]Z level=ERROR source=write_handler.go:[VAR] msg=\"Error appending remote write\" component=web err=\"too old sample\"`
  - representatives:
    - 1:
      - ts_ms: 1775078919540
      - message: `time=2026-04-01T21:28:39.540Z level=ERROR source=write_handler.go:653 msg=\"Error appending remote write\" component=web err=\"too old sample\"`";

        let sections = analyze_markdown_structure(md).unwrap();
        assert_eq!(sections.len(), 3);

        // Root Cause Analysis: line 1-4
        assert_eq!(sections[0].title, "Root Cause Analysis");
        assert_eq!(
            sections[0].keys,
            vec!["version", "window_ms", "earliest_observed_ts_ms"]
        );

        // Summary: line 6 to before Groups
        assert_eq!(sections[1].title, "Summary");
        assert_eq!(
            sections[1].keys,
            vec![
                "total_groups",
                "total_logs_weighted",
                "by_level",
                "top_apps"
            ]
        );

        // Groups: line 16 to EOF
        assert_eq!(sections[2].title, "Groups");
        assert_eq!(sections[2].keys, vec!["group", "group"]);

        // Verify the formatted output gives useful navigation hints
        let formatted = format_markdown_schema(&sections, 10);
        assert!(formatted.contains("Root Cause Analysis"));
        assert!(formatted.contains("Summary"));
        assert!(formatted.contains("Groups"));
        // All sections should show line ranges
        assert!(formatted.contains("[L"));
    }

    #[test]
    fn test_markdown_nested_headers() {
        let md = "# Top\n## Sub A\n- key: val\n## Sub B\n- key2: val\n";
        let sections = analyze_markdown_structure(md).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "Top");
        assert_eq!(sections[0].children.len(), 2);
        assert_eq!(sections[0].children[0].title, "Sub A");
        assert_eq!(sections[0].children[1].title, "Sub B");
    }
}
