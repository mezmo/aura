use anyhow::{Result, anyhow};
use regex::RegexBuilder;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::permissions::glob_match;

/// Execute a tool call by name. Returns the result string.
/// Note: CompactContext is handled directly in the REPL loop because it
/// needs access to ConversationHistory. It should never reach here.
pub fn execute_tool(name: &str, arguments: &str) -> Result<String> {
    match name {
        "Shell" => execute_shell(arguments),
        "Read" => execute_read(arguments),
        "ListFiles" => execute_list_files(arguments),
        "SearchFiles" => execute_search_files(arguments),
        "FindFiles" => execute_find_files(arguments),
        "FileInfo" => execute_file_info(arguments),
        "CompactContext" => Ok("CompactContext should be handled by the REPL loop".to_string()),
        "Update" => Ok("Update context started. Use Shell calls to make changes.".to_string()),
        _ => Ok(format!("Unknown tool: {name}")),
    }
}

/// Execute a shell command and return stdout/stderr/exit code.
fn execute_shell(arguments: &str) -> Result<String> {
    let args: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or_else(|_| json!({"command": arguments}));

    let command = args["command"].as_str().unwrap_or(arguments);

    let output = Command::new("sh").arg("-c").arg(command).output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    if exit_code == 0 && stderr.is_empty() {
        Ok(stdout.to_string())
    } else {
        Ok(format!(
            "stdout:\n{stdout}\nstderr:\n{stderr}\nexit_code: {exit_code}"
        ))
    }
}

/// Read a file with chunked streaming (offset/limit).
fn execute_read(arguments: &str) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let file_path = args["file_path"]
        .as_str()
        .ok_or_else(|| anyhow!("missing file_path"))?;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(500) as usize;

    let content = std::fs::read_to_string(file_path)?;
    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();

    let chunk: Vec<String> = all_lines
        .iter()
        .skip(offset)
        .take(limit)
        .enumerate()
        .map(|(i, line)| format!("{:>6}\u{2502}{}", offset + i + 1, line))
        .collect();

    let returned = chunk.len();
    let has_more = offset + returned < total_lines;
    let next_offset = offset + returned;

    Ok(format!(
        "[lines {}-{} of {} total | has_more: {} | next_offset: {}]\n{}",
        offset + 1,
        offset + returned,
        total_lines,
        has_more,
        next_offset,
        chunk.join("\n")
    ))
}

/// List files and directories at a given path (single directory, non-recursive).
fn execute_list_files(arguments: &str) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("missing path"))?;

    let dir = std::path::Path::new(path);
    if !dir.exists() {
        return Ok(format!("Error: path does not exist: {path}"));
    }
    if !dir.is_dir() {
        return Ok(format!("Error: path is not a directory: {path}"));
    }

    let mut entries = Vec::new();
    collect_entries_flat(dir, &mut entries)?;

    if entries.is_empty() {
        return Ok(format!("[{path}: empty directory]"));
    }

    let total = entries.len();
    let mut output = entries.join("\n");
    output.push_str(&format!("\n[{total} entries]"));

    Ok(output)
}

/// Collect entries for a single directory (non-recursive).
fn collect_entries_flat(dir: &std::path::Path, entries: &mut Vec<String>) -> Result<()> {
    let mut dir_entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    dir_entries.sort_by_key(|e| e.file_name());

    for entry in dir_entries {
        let meta = entry.metadata()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let kind = if meta.is_dir() {
            "dir"
        } else if meta.is_symlink() {
            "symlink"
        } else {
            "file"
        };
        let size = if meta.is_file() {
            format_size(meta.len())
        } else {
            "-".to_string()
        };
        entries.push(format!("{kind:>7}  {size:>8}  {name}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Directories to skip during recursive walks
// ---------------------------------------------------------------------------

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    "vendor",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    ".eggs",
    ".bundle",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
];

/// Returns true if a directory entry name should be skipped during recursive walks.
fn is_skip_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

/// Check if a file appears to be binary by looking for null bytes in the first 8192 bytes.
fn is_binary_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 8192];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    buf[..n].contains(&0)
}

/// Recursively walk a directory, calling `visitor` for each entry.
/// Skips hidden/vendor directories and doesn't follow symlinks into directories.
fn walk_directory(
    root: &Path,
    max_depth: Option<usize>,
    visitor: &mut dyn FnMut(&Path, &std::fs::Metadata, usize) -> bool,
) -> Result<()> {
    walk_directory_inner(root, 0, max_depth, visitor)
}

fn walk_directory_inner(
    dir: &Path,
    depth: usize,
    max_depth: Option<usize>,
    visitor: &mut dyn FnMut(&Path, &std::fs::Metadata, usize) -> bool,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()), // skip unreadable dirs
    };

    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };

        // Call visitor; if it returns false, stop walking entirely
        if !visitor(&path, &meta, depth) {
            return Ok(());
        }

        // Recurse into directories (but not symlinks to directories)
        if meta.is_dir() && !meta.file_type().is_symlink() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_skip_dir(&name) {
                continue;
            }
            if let Some(max) = max_depth
                && depth + 1 > max
            {
                continue;
            }
            walk_directory_inner(&path, depth + 1, max_depth, visitor)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FileInfo
// ---------------------------------------------------------------------------

fn execute_file_info(arguments: &str) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let path_str = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("missing path"))?;
    let path = Path::new(path_str);

    if !path.exists() {
        return Ok(format!("Error: path does not exist: {path_str}"));
    }

    let meta = std::fs::metadata(path)?;

    if meta.is_file() {
        let size = meta.len();
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| {
                let duration = t.duration_since(std::time::UNIX_EPOCH).ok()?;
                Some(format_timestamp(duration.as_secs()))
            })
            .unwrap_or_else(|| "unknown".to_string());

        // Count lines
        let line_count = match std::fs::read_to_string(path) {
            Ok(content) => content.lines().count(),
            Err(_) => 0, // binary or unreadable
        };

        let is_binary = is_binary_file(path);

        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            format!("{:o}", meta.permissions().mode() & 0o777)
        };
        #[cfg(not(unix))]
        let permissions = if meta.permissions().readonly() {
            "readonly".to_string()
        } else {
            "read-write".to_string()
        };

        Ok(format!(
            "type: file\n\
             size: {} ({} bytes)\n\
             lines: {}\n\
             binary: {}\n\
             modified: {}\n\
             permissions: {}",
            format_size(size),
            size,
            line_count,
            is_binary,
            modified,
            permissions,
        ))
    } else if meta.is_dir() {
        let mut file_count = 0usize;
        let mut dir_count = 0usize;

        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Ok(m) = entry.metadata() {
                    if m.is_dir() {
                        dir_count += 1;
                    } else {
                        file_count += 1;
                    }
                }
            }
        }

        let modified = meta
            .modified()
            .ok()
            .and_then(|t| {
                let duration = t.duration_since(std::time::UNIX_EPOCH).ok()?;
                Some(format_timestamp(duration.as_secs()))
            })
            .unwrap_or_else(|| "unknown".to_string());

        Ok(format!(
            "type: directory\n\
             entries: {} ({} files, {} directories)\n\
             modified: {}",
            file_count + dir_count,
            file_count,
            dir_count,
            modified,
        ))
    } else {
        Ok(format!("type: symlink\ntarget: {}", path.display()))
    }
}

/// Format a Unix timestamp as a human-readable datetime string.
fn format_timestamp(secs: u64) -> String {
    // Simple UTC formatting without pulling in chrono
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to Y-M-D (simplified Gregorian)
    let (year, month, day) = days_to_ymd(days);

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// FindFiles
// ---------------------------------------------------------------------------

fn execute_find_files(arguments: &str) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let path_str = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("missing path"))?;
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow!("missing pattern"))?;
    let type_filter = args["type_filter"].as_str().unwrap_or("any");
    let max_depth = args["max_depth"].as_u64().map(|d| d as usize);
    let max_results = args["max_results"].as_u64().unwrap_or(200) as usize;

    let root = Path::new(path_str);
    if !root.exists() {
        return Ok(format!("Error: path does not exist: {path_str}"));
    }
    if !root.is_dir() {
        return Ok(format!("Error: path is not a directory: {path_str}"));
    }

    let mut results = Vec::new();
    let mut truncated = false;

    walk_directory(root, max_depth, &mut |path, meta, _depth| {
        if results.len() >= max_results {
            truncated = true;
            return false; // stop walking
        }

        let file_name = match path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => return true,
        };

        // Apply glob pattern to the file name
        if !glob_match(pattern, &file_name) {
            return true; // continue
        }

        // Apply type filter
        let is_dir = meta.is_dir();
        let is_file = meta.is_file();
        match type_filter {
            "file" if !is_file => return true,
            "dir" if !is_dir => return true,
            _ => {}
        }

        let kind = if is_dir {
            "dir"
        } else if meta.file_type().is_symlink() {
            "symlink"
        } else {
            "file"
        };
        let size = if is_file {
            format_size(meta.len())
        } else {
            "-".to_string()
        };
        let display_path = path.display();
        results.push(format!("{kind:>7}  {size:>8}  {display_path}"));

        true // continue
    })?;

    if results.is_empty() {
        return Ok(format!(
            "[No matches for pattern \"{pattern}\" in {path_str}]"
        ));
    }

    let count = results.len();
    let mut output = results.join("\n");
    if truncated {
        output.push_str(&format!(
            "\n[{count} results — truncated at max_results={max_results}]"
        ));
    } else {
        output.push_str(&format!("\n[{count} results]"));
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// SearchFiles
// ---------------------------------------------------------------------------

fn execute_search_files(arguments: &str) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let pattern_str = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow!("missing pattern"))?;
    let path_str = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("missing path"))?;
    let is_regex = args["regex"].as_bool().unwrap_or(false);
    let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(true);
    let lines_before = args["lines_before"].as_u64().unwrap_or(0) as usize;
    let lines_after = args["lines_after"].as_u64().unwrap_or(0) as usize;
    let include_pattern = args["include_pattern"].as_str();
    let max_results = args["max_results"].as_u64().unwrap_or(100) as usize;

    // Build regex
    let regex_pattern = if is_regex {
        pattern_str.to_string()
    } else {
        regex::escape(pattern_str)
    };
    let re = RegexBuilder::new(&regex_pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| anyhow!("invalid regex: {e}"))?;

    let root = Path::new(path_str);
    if !root.exists() {
        return Ok(format!("Error: path does not exist: {path_str}"));
    }

    // Collect files to search
    let mut files_to_search: Vec<PathBuf> = Vec::new();
    if root.is_file() {
        files_to_search.push(root.to_path_buf());
    } else {
        walk_directory(root, None, &mut |path, meta, _depth| {
            if meta.is_file() {
                // Apply include_pattern filter to file name
                if let Some(inc) = include_pattern
                    && let Some(name) = path.file_name()
                    && !glob_match(inc, &name.to_string_lossy())
                {
                    return true;
                }
                files_to_search.push(path.to_path_buf());
            }
            true
        })?;
    }

    let mut output_lines: Vec<String> = Vec::new();
    let mut match_count = 0usize;
    let mut files_with_matches = 0usize;
    let mut truncated = false;

    'outer: for file_path in &files_to_search {
        // Skip binary files
        if is_binary_file(file_path) {
            continue;
        }

        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue, // skip unreadable files
        };

        let all_lines: Vec<&str> = content.lines().collect();

        // Find all matching line indices
        let mut match_indices: Vec<usize> = Vec::new();
        for (i, line) in all_lines.iter().enumerate() {
            if re.is_match(line) {
                match_indices.push(i);
            }
        }

        if match_indices.is_empty() {
            continue;
        }

        files_with_matches += 1;
        let display_path = file_path.display();

        // Merge overlapping context windows
        let ranges =
            merge_context_ranges(&match_indices, lines_before, lines_after, all_lines.len());

        for range in &ranges {
            for (i, line) in all_lines
                .iter()
                .enumerate()
                .take(range.end)
                .skip(range.start)
            {
                if match_count >= max_results {
                    truncated = true;
                    break 'outer;
                }

                let line = *line;
                let truncated_line = if line.len() > 500 {
                    format!("{}...", &line[..500])
                } else {
                    line.to_string()
                };

                let is_match = match_indices.contains(&i);
                let line_num = i + 1; // 1-based

                if is_match {
                    output_lines.push(format!("{display_path}:{line_num}:{truncated_line}"));
                    match_count += 1;
                } else {
                    // Context line
                    output_lines.push(format!("{display_path}-{line_num}-{truncated_line}"));
                }
            }

            // Separator between non-contiguous ranges in the same file
            if ranges.len() > 1 {
                output_lines.push("--".to_string());
            }
        }

        // Remove trailing separator if we added one
        if !ranges.is_empty() && output_lines.last().map(|s| s.as_str()) == Some("--") {
            output_lines.pop();
        }
    }

    if output_lines.is_empty() {
        return Ok(format!("[No matches for \"{pattern_str}\" in {path_str}]"));
    }

    let mut header = format!(
        "[{match_count} matches in {files_with_matches} {}",
        if files_with_matches == 1 {
            "file"
        } else {
            "files"
        }
    );
    if truncated {
        header.push_str(&format!(" — truncated at max_results={max_results}"));
    }
    header.push(']');

    let body = output_lines.join("\n");
    Ok(format!("{header}\n{body}"))
}

/// A range of line indices [start, end) to display.
struct LineRange {
    start: usize,
    end: usize,
}

/// Merge overlapping context windows around match indices into contiguous ranges.
fn merge_context_ranges(
    match_indices: &[usize],
    before: usize,
    after: usize,
    total_lines: usize,
) -> Vec<LineRange> {
    if match_indices.is_empty() {
        return Vec::new();
    }

    let mut ranges: Vec<LineRange> = Vec::new();

    for &idx in match_indices {
        let start = idx.saturating_sub(before);
        let end = (idx + after + 1).min(total_lines);

        if let Some(last) = ranges.last_mut()
            && start <= last.end
        {
            // Overlapping or adjacent — merge
            last.end = last.end.max(end);
            continue;
        }
        ranges.push(LineRange { start, end });
    }

    ranges
}

/// Format a byte size as a human-readable string.
pub(crate) fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // execute_tool dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn execute_tool_unknown_returns_message() {
        let result = execute_tool("FakeTool", "{}").unwrap();
        assert!(result.contains("Unknown tool: FakeTool"));
    }

    #[test]
    fn execute_tool_compact_context_returns_message() {
        let result = execute_tool("CompactContext", "{}").unwrap();
        assert!(result.contains("REPL loop"));
    }

    #[test]
    fn execute_tool_update_returns_message() {
        let result = execute_tool("Update", r#"{"file_path":"test.txt"}"#).unwrap();
        assert!(result.contains("Update context started"));
    }

    // -----------------------------------------------------------------------
    // execute_shell
    // -----------------------------------------------------------------------

    #[test]
    fn execute_shell_echo() {
        let result = execute_shell(r#"{"command":"echo hello"}"#).unwrap();
        assert_eq!(result.trim(), "hello");
    }

    #[test]
    fn execute_shell_exit_code() {
        let result = execute_shell(r#"{"command":"exit 1"}"#).unwrap();
        assert!(result.contains("exit_code: 1"));
    }

    #[test]
    fn execute_shell_stderr() {
        let result = execute_shell(r#"{"command":"echo err >&2"}"#).unwrap();
        assert!(result.contains("stderr:"));
        assert!(result.contains("err"));
    }

    // -----------------------------------------------------------------------
    // execute_read
    // -----------------------------------------------------------------------

    #[test]
    fn execute_read_basic() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let args = format!(r#"{{"file_path":"{}"}}"#, file_path.display());
        let result = execute_read(&args).unwrap();

        assert!(result.contains("lines 1-3 of 3 total"));
        assert!(result.contains("has_more: false"));
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
    }

    #[test]
    fn execute_read_with_offset_and_limit() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        let content: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        fs::write(&file_path, &content).unwrap();

        let args = format!(
            r#"{{"file_path":"{}","offset":5,"limit":3}}"#,
            file_path.display()
        );
        let result = execute_read(&args).unwrap();

        assert!(result.contains("lines 6-8 of 20 total"));
        assert!(result.contains("has_more: true"));
        assert!(result.contains("next_offset: 8"));
        assert!(result.contains("line6"));
    }

    #[test]
    fn execute_read_missing_file() {
        let result = execute_read(r#"{"file_path":"/nonexistent/file.txt"}"#);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // execute_list_files
    // -----------------------------------------------------------------------

    #[test]
    fn execute_list_files_basic() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let args = format!(r#"{{"path":"{}"}}"#, dir.path().display());
        let result = execute_list_files(&args).unwrap();

        assert!(result.contains("a.txt"));
        assert!(result.contains("subdir"));
        assert!(result.contains("file"));
        assert!(result.contains("dir"));
        assert!(result.contains("[2 entries]"));
    }

    #[test]
    fn execute_list_files_nonexistent() {
        let result = execute_list_files(r#"{"path":"/nonexistent/dir"}"#).unwrap();
        assert!(result.contains("Error: path does not exist"));
    }

    #[test]
    fn execute_list_files_empty_dir() {
        let dir = TempDir::new().unwrap();
        let args = format!(r#"{{"path":"{}"}}"#, dir.path().display());
        let result = execute_list_files(&args).unwrap();
        assert!(result.contains("empty directory"));
    }

    // -----------------------------------------------------------------------
    // execute_find_files
    // -----------------------------------------------------------------------

    #[test]
    fn execute_find_files_glob() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("foo.rs"), "").unwrap();
        fs::write(dir.path().join("bar.rs"), "").unwrap();
        fs::write(dir.path().join("baz.txt"), "").unwrap();

        let args = format!(r#"{{"path":"{}","pattern":"*.rs"}}"#, dir.path().display());
        let result = execute_find_files(&args).unwrap();

        assert!(result.contains("foo.rs"));
        assert!(result.contains("bar.rs"));
        assert!(!result.contains("baz.txt"));
        assert!(result.contains("[2 results]"));
    }

    #[test]
    fn execute_find_files_type_filter() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file.txt"), "").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let args = format!(
            r#"{{"path":"{}","pattern":"*","type_filter":"dir"}}"#,
            dir.path().display()
        );
        let result = execute_find_files(&args).unwrap();

        assert!(result.contains("subdir"));
        assert!(!result.contains("file.txt"));
    }

    #[test]
    fn execute_find_files_max_results() {
        let dir = TempDir::new().unwrap();
        for i in 0..10 {
            fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }

        let args = format!(
            r#"{{"path":"{}","pattern":"*.txt","max_results":3}}"#,
            dir.path().display()
        );
        let result = execute_find_files(&args).unwrap();

        assert!(result.contains("truncated"));
        assert!(result.contains("[3 results"));
    }

    #[test]
    fn execute_find_files_no_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("foo.txt"), "").unwrap();

        let args = format!(r#"{{"path":"{}","pattern":"*.rs"}}"#, dir.path().display());
        let result = execute_find_files(&args).unwrap();
        assert!(result.contains("No matches"));
    }

    // -----------------------------------------------------------------------
    // execute_search_files
    // -----------------------------------------------------------------------

    #[test]
    fn execute_search_files_literal() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.rs"), "fn main() {\n    hello()\n}\n").unwrap();

        let args = format!(r#"{{"pattern":"hello","path":"{}"}}"#, dir.path().display());
        let result = execute_search_files(&args).unwrap();

        assert!(result.contains("1 matches in 1 file"));
        assert!(result.contains("hello()"));
    }

    #[test]
    fn execute_search_files_regex() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();

        let args = format!(
            r#"{{"pattern":"fn \\w+","path":"{}","regex":true}}"#,
            dir.path().display()
        );
        let result = execute_search_files(&args).unwrap();

        assert!(result.contains("2 matches"));
    }

    #[test]
    fn execute_search_files_case_insensitive() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "Hello\nhello\nHELLO\n").unwrap();

        let args = format!(
            r#"{{"pattern":"hello","path":"{}","case_sensitive":false}}"#,
            dir.path().display()
        );
        let result = execute_search_files(&args).unwrap();

        assert!(result.contains("3 matches"));
    }

    #[test]
    fn execute_search_files_include_pattern() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        fs::write(dir.path().join("b.txt"), "needle\n").unwrap();

        let args = format!(
            r#"{{"pattern":"needle","path":"{}","include_pattern":"*.rs"}}"#,
            dir.path().display()
        );
        let result = execute_search_files(&args).unwrap();

        assert!(result.contains("1 matches in 1 file"));
        assert!(result.contains("a.rs"));
    }

    #[test]
    fn execute_search_files_context_lines() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "aaa\nbbb\nccc\nddd\neee\n").unwrap();

        let args = format!(
            r#"{{"pattern":"ccc","path":"{}","lines_before":1,"lines_after":1}}"#,
            dir.path().display()
        );
        let result = execute_search_files(&args).unwrap();

        assert!(result.contains("bbb")); // context before
        assert!(result.contains("ccc")); // match
        assert!(result.contains("ddd")); // context after
    }

    #[test]
    fn execute_search_files_max_results() {
        let dir = TempDir::new().unwrap();
        let content: String = (0..50).map(|i| format!("match_{i}\n")).collect();
        fs::write(dir.path().join("test.txt"), &content).unwrap();

        let args = format!(
            r#"{{"pattern":"match_","path":"{}","max_results":5}}"#,
            dir.path().display()
        );
        let result = execute_search_files(&args).unwrap();

        assert!(result.contains("truncated"));
        assert!(result.contains("5 matches"));
    }

    #[test]
    fn execute_search_files_no_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "nothing here\n").unwrap();

        let args = format!(
            r#"{{"pattern":"nonexistent","path":"{}"}}"#,
            dir.path().display()
        );
        let result = execute_search_files(&args).unwrap();
        assert!(result.contains("No matches"));
    }

    // -----------------------------------------------------------------------
    // execute_file_info
    // -----------------------------------------------------------------------

    #[test]
    fn execute_file_info_on_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let args = format!(r#"{{"path":"{}"}}"#, file_path.display());
        let result = execute_file_info(&args).unwrap();

        assert!(result.contains("type: file"));
        assert!(result.contains("lines: 3"));
        assert!(result.contains("binary: false"));
    }

    #[test]
    fn execute_file_info_on_dir() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();

        let args = format!(r#"{{"path":"{}"}}"#, dir.path().display());
        let result = execute_file_info(&args).unwrap();

        assert!(result.contains("type: directory"));
        assert!(result.contains("2 files"));
        assert!(result.contains("1 directories"));
    }

    #[test]
    fn execute_file_info_nonexistent() {
        let result = execute_file_info(r#"{"path":"/nonexistent/path"}"#).unwrap();
        assert!(result.contains("Error: path does not exist"));
    }

    // -----------------------------------------------------------------------
    // format_size
    // -----------------------------------------------------------------------

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kb() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn format_size_mb() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn format_size_gb() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    // -----------------------------------------------------------------------
    // days_to_ymd
    // -----------------------------------------------------------------------

    #[test]
    fn days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2020-01-01 is day 18262 from epoch
        assert_eq!(days_to_ymd(18262), (2020, 1, 1));
    }

    #[test]
    fn days_to_ymd_another_known_date() {
        // 2000-03-01 is day 11017 from epoch
        assert_eq!(days_to_ymd(11017), (2000, 3, 1));
    }

    // -----------------------------------------------------------------------
    // format_timestamp
    // -----------------------------------------------------------------------

    #[test]
    fn format_timestamp_epoch() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
    }

    #[test]
    fn format_timestamp_known() {
        // 2020-01-01 00:00:00 UTC = 1577836800
        assert_eq!(format_timestamp(1577836800), "2020-01-01 00:00:00 UTC");
    }

    // -----------------------------------------------------------------------
    // merge_context_ranges
    // -----------------------------------------------------------------------

    #[test]
    fn merge_context_ranges_empty() {
        let result = merge_context_ranges(&[], 1, 1, 10);
        assert!(result.is_empty());
    }

    #[test]
    fn merge_context_ranges_single_no_context() {
        let result = merge_context_ranges(&[5], 0, 0, 10);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start, 5);
        assert_eq!(result[0].end, 6);
    }

    #[test]
    fn merge_context_ranges_single_with_context() {
        let result = merge_context_ranges(&[5], 2, 2, 10);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start, 3);
        assert_eq!(result[0].end, 8);
    }

    #[test]
    fn merge_context_ranges_overlapping() {
        let result = merge_context_ranges(&[3, 5], 1, 1, 10);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start, 2);
        assert_eq!(result[0].end, 7);
    }

    #[test]
    fn merge_context_ranges_non_overlapping() {
        let result = merge_context_ranges(&[1, 8], 0, 0, 10);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn merge_context_ranges_clamped_to_bounds() {
        let result = merge_context_ranges(&[0], 5, 5, 3);
        assert_eq!(result[0].start, 0);
        assert_eq!(result[0].end, 3);
    }

    // -----------------------------------------------------------------------
    // is_skip_dir
    // -----------------------------------------------------------------------

    #[test]
    fn is_skip_dir_hidden() {
        assert!(is_skip_dir(".git"));
        assert!(is_skip_dir(".hidden"));
    }

    #[test]
    fn is_skip_dir_known() {
        assert!(is_skip_dir("node_modules"));
        assert!(is_skip_dir("target"));
        assert!(is_skip_dir("__pycache__"));
    }

    #[test]
    fn is_skip_dir_normal() {
        assert!(!is_skip_dir("src"));
        assert!(!is_skip_dir("lib"));
    }

    // -----------------------------------------------------------------------
    // is_binary_file
    // -----------------------------------------------------------------------

    #[test]
    fn is_binary_file_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("text.txt");
        fs::write(&path, "hello world\n").unwrap();
        assert!(!is_binary_file(&path));
    }

    #[test]
    fn is_binary_file_with_null() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("binary.bin");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&[0x48, 0x65, 0x6c, 0x00, 0x6f]).unwrap();
        assert!(is_binary_file(&path));
    }

    #[test]
    fn is_binary_file_nonexistent() {
        assert!(!is_binary_file(Path::new("/nonexistent/file")));
    }
}
