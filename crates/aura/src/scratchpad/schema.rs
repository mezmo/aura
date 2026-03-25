//! JSON structure traversal with line ranges.
//!
//! Provides a schema-like overview of JSON data, showing keys, types,
//! array lengths, and the line ranges where each element appears.

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
}
