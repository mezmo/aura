use crossterm::style::Stylize;
use std::process::Command;

/// Compute a unified diff between old and new content.
/// Uses the system `diff` command with fallback to simple line comparison.
/// Returns (diff_text, lines_added, lines_removed).
pub fn compute_diff(old_content: &str, new_content: &str) -> (String, usize, usize) {
    if let Some(result) = compute_diff_system(old_content, new_content) {
        return result;
    }
    compute_diff_fallback(old_content, new_content)
}

fn compute_diff_system(old_content: &str, new_content: &str) -> Option<(String, usize, usize)> {
    let tmp_dir = std::env::temp_dir();
    // Unique per call so concurrent Update operations and multiple CLI processes
    // don't clobber each other's temp files (and to avoid symlink-bait at predictable paths).
    let id = uuid::Uuid::new_v4();
    let old_path = tmp_dir.join(format!("aura_diff_{id}_old.tmp"));
    let new_path = tmp_dir.join(format!("aura_diff_{id}_new.tmp"));

    std::fs::write(&old_path, old_content).ok()?;
    std::fs::write(&new_path, new_content).ok()?;

    let output = Command::new("diff")
        .arg("-u")
        .arg(&old_path)
        .arg(&new_path)
        .output()
        .ok()?;

    let _ = std::fs::remove_file(&old_path);
    let _ = std::fs::remove_file(&new_path);

    let diff_output = String::from_utf8_lossy(&output.stdout).to_string();

    // diff returns exit code 1 when files differ (not an error)
    // exit code 0 means no difference, exit code 2+ means error
    if output.status.code().unwrap_or(2) > 1 {
        return None;
    }

    let mut lines_added = 0usize;
    let mut lines_removed = 0usize;
    for line in diff_output.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            lines_added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            lines_removed += 1;
        }
    }

    Some((diff_output, lines_added, lines_removed))
}

fn compute_diff_fallback(old_content: &str, new_content: &str) -> (String, usize, usize) {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    let mut diff_text = String::new();
    let mut lines_added = 0usize;
    let mut lines_removed = 0usize;

    // Simple approach: use longest common subsequence length to count changes,
    // then show removed and added lines
    let old_set: std::collections::HashSet<&str> = old_lines.iter().copied().collect();
    let new_set: std::collections::HashSet<&str> = new_lines.iter().copied().collect();

    for line in &old_lines {
        if !new_set.contains(line) {
            diff_text.push_str(&format!("-{line}\n"));
            lines_removed += 1;
        }
    }
    for line in &new_lines {
        if !old_set.contains(line) {
            diff_text.push_str(&format!("+{line}\n"));
            lines_added += 1;
        }
    }

    (diff_text, lines_added, lines_removed)
}

/// Parse a unified diff `@@ -old_start,count +new_start,count @@` header.
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let old_part = parts[1].trim_start_matches('-');
    let new_part = parts[2].trim_start_matches('+');
    let old_start: usize = old_part.split(',').next()?.parse().ok()?;
    let new_start: usize = new_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

/// Print diff lines with colored output, line numbers, and indentation.
/// `-` lines are red, `+` lines are blue.
/// Diff lines use plain space indentation (no `⎿` prefix).
/// `max_diff_lines`: max +/- lines to show (0 = unlimited).
pub fn print_update_diff(diff_text: &str, max_diff_lines: usize) {
    // Indentation for diff lines
    let indent = "   ";

    // Pre-scan to find max line number for right-aligning
    let mut max_line_num: usize = 0;
    let mut old_line: usize = 1;
    let mut new_line: usize = 1;

    for line in diff_text.lines() {
        if line.starts_with("@@") {
            if let Some((old_start, new_start)) = parse_hunk_header(line) {
                old_line = old_start;
                new_line = new_start;
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            max_line_num = max_line_num.max(old_line);
            old_line += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            max_line_num = max_line_num.max(new_line);
            new_line += 1;
        } else if !line.starts_with("---") && !line.starts_with("+++") {
            old_line += 1;
            new_line += 1;
        }
    }

    let num_width = max_line_num.to_string().len().max(4);

    // Second pass: display
    old_line = 1;
    new_line = 1;
    let mut shown = 0usize;
    let mut remaining = 0usize;
    let mut counting_remaining = false;

    for line in diff_text.lines() {
        if line.starts_with("---") || line.starts_with("+++") {
            continue;
        }
        if line.starts_with("@@") {
            if let Some((old_start, new_start)) = parse_hunk_header(line) {
                old_line = old_start;
                new_line = new_start;
            }
            continue;
        }

        if let Some(content) = line.strip_prefix('-') {
            if counting_remaining {
                remaining += 1;
                old_line += 1;
                continue;
            }
            if max_diff_lines > 0 && shown >= max_diff_lines {
                counting_remaining = true;
                remaining += 1;
                old_line += 1;
                continue;
            }

            println!(
                "{}{}",
                indent,
                format!("{:>width$} -{content}", old_line, width = num_width)
                    .with(crossterm::style::Color::Red),
            );
            old_line += 1;
            shown += 1;
        } else if let Some(content) = line.strip_prefix('+') {
            if counting_remaining {
                remaining += 1;
                new_line += 1;
                continue;
            }
            if max_diff_lines > 0 && shown >= max_diff_lines {
                counting_remaining = true;
                remaining += 1;
                new_line += 1;
                continue;
            }

            println!(
                "{}{}",
                indent,
                format!("{:>width$} +{content}", new_line, width = num_width)
                    .with(crossterm::style::Color::Blue),
            );
            new_line += 1;
            shown += 1;
        } else {
            // Context line
            old_line += 1;
            new_line += 1;
        }
    }

    if remaining > 0 {
        println!(
            "{}{}",
            indent,
            format!("... ({remaining} more lines)").with(crossterm::style::Color::DarkGrey),
        );
    }
}

/// Print the Update summary line (e.g. "Added 3 lines, removed 1 line").
pub fn print_update_summary(lines_added: usize, lines_removed: usize, connector: &str) {
    let added_word = if lines_added == 1 { "line" } else { "lines" };
    let removed_word = if lines_removed == 1 { "line" } else { "lines" };
    println!(
        "{} {}",
        connector.with(crossterm::style::Color::DarkGrey),
        format!("Added {lines_added} {added_word}, removed {lines_removed} {removed_word}")
            .with(crossterm::style::Color::DarkGrey),
    );
}

/// Print the "Using cmd1, cmd2 to perform updates" line.
pub fn print_update_commands_summary(commands: &[String], expanded: bool, connector: &str) {
    let cmds = if commands.is_empty() {
        "shell".to_string()
    } else {
        commands.join(", ")
    };
    let suffix = if !expanded {
        " - ... /expand to show more"
    } else {
        ""
    };
    println!(
        "{} {}",
        connector.with(crossterm::style::Color::DarkGrey),
        format!("Using {cmds} to perform updates{suffix}").with(crossterm::style::Color::DarkGrey),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // compute_diff_fallback
    // -----------------------------------------------------------------------

    #[test]
    fn compute_diff_fallback_identical() {
        let (diff, added, removed) = compute_diff_fallback("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
        assert!(diff.is_empty());
    }

    #[test]
    fn compute_diff_fallback_additions() {
        let (diff, added, removed) = compute_diff_fallback("a\n", "a\nb\nc\n");
        assert_eq!(added, 2);
        assert_eq!(removed, 0);
        assert!(diff.contains("+b"));
        assert!(diff.contains("+c"));
    }

    #[test]
    fn compute_diff_fallback_removals() {
        let (diff, added, removed) = compute_diff_fallback("a\nb\nc\n", "a\n");
        assert_eq!(removed, 2);
        assert_eq!(added, 0);
        assert!(diff.contains("-b"));
        assert!(diff.contains("-c"));
    }

    #[test]
    fn compute_diff_fallback_mixed() {
        let (_, added, removed) = compute_diff_fallback("a\nb\n", "a\nc\n");
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
    }

    // -----------------------------------------------------------------------
    // compute_diff (uses system diff)
    // -----------------------------------------------------------------------

    #[test]
    fn compute_diff_identical() {
        let (_, added, removed) = compute_diff("hello\n", "hello\n");
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn compute_diff_system_changes() {
        let result = compute_diff_system("aaa\nbbb\n", "aaa\nccc\n");
        if let Some((diff, added, removed)) = result {
            assert_eq!(added, 1);
            assert_eq!(removed, 1);
            assert!(diff.contains("-bbb"));
            assert!(diff.contains("+ccc"));
        }
        // If system diff is unavailable, that's ok — fallback is tested separately
    }

    #[test]
    fn compute_diff_dispatches() {
        // compute_diff should return correct results regardless of which path it takes
        let (_, added, removed) = compute_diff("aaa\nbbb\n", "aaa\nccc\n");
        assert!(added >= 1, "expected at least 1 addition, got {added}");
        assert!(removed >= 1, "expected at least 1 removal, got {removed}");
    }

    // -----------------------------------------------------------------------
    // parse_hunk_header
    // -----------------------------------------------------------------------

    #[test]
    fn parse_hunk_header_standard() {
        let result = parse_hunk_header("@@ -1,5 +1,7 @@");
        assert_eq!(result, Some((1, 1)));
    }

    #[test]
    fn parse_hunk_header_different_starts() {
        let result = parse_hunk_header("@@ -10,3 +15,4 @@");
        assert_eq!(result, Some((10, 15)));
    }

    #[test]
    fn parse_hunk_header_invalid() {
        assert_eq!(parse_hunk_header("not a hunk"), None);
    }
}
