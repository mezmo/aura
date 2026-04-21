use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use crossterm::style::Stylize;
use serde::Deserialize;

use crate::aura_dir::find_project_aura_dir_with_home;

/// Filename for the machine-mutated permissions file. Lives at
/// `<project>/.aura/permissions.json`. Named `permissions.json` (rather
/// than the older `settings.json`) so the file's purpose is obvious and it
/// can never be mistaken for the human-edited `cli.toml`.
const PERMISSIONS_FILENAME: &str = "permissions.json";

/// Pre-rename filename. Read with a deprecation warning if
/// `permissions.json` is absent from the same `.aura/` directory; new
/// writes always go to `permissions.json`.
const LEGACY_PERMISSIONS_FILENAME: &str = "settings.json";

/// Result of a permission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult {
    /// An explicit allow rule matched — execute immediately.
    Allowed,
    /// An explicit deny rule matched — block with reason.
    Denied(String),
    /// No rule matched — prompt the user before executing.
    Prompt,
}

#[derive(Debug, Clone)]
pub struct PermissionChecker {
    allow: Vec<PermissionRule>,
    deny: Vec<PermissionRule>,
    /// Where to write `permissions.json` when the user accepts an "always
    /// allow" / "deny always" choice. Set to the project root discovered
    /// by the `.aura/` walk-up if any, otherwise to the cwd at load time
    /// (in which case `.aura/permissions.json` will be created on first
    /// save). The actual file lives at `save_dir.join(".aura/permissions.json")`.
    save_dir: PathBuf,
    /// Tool names for which we've already shown the wildcard hint,
    /// so we only suggest it once per session per tool.
    wildcard_hint_shown: HashSet<String>,
}

/// Number of path-specific allow rules for a single tool before we suggest
/// the user consolidate them with a wildcard in `.aura/permissions.json`.
const WILDCARD_HINT_THRESHOLD: usize = 3;

impl Default for PermissionChecker {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            save_dir: PathBuf::from("."),
            wildcard_hint_shown: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct PermissionRule {
    tool_name: String,
    argument_pattern: String,
}

#[derive(Deserialize)]
struct SettingsFile {
    #[serde(default)]
    permissions: PermissionsConfig,
}

#[derive(Deserialize, Default)]
struct PermissionsConfig {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

impl PermissionChecker {
    /// Load permissions for the given working directory.
    ///
    /// Walks up from `cwd` looking for the closest `.aura/permissions.json`
    /// (or the legacy `settings.json` with a deprecation warning). There is
    /// **no global permissions file** — by design, permissions live only
    /// in a project's `.aura/` so the user can always answer "what
    /// permissions am I running with?" by looking at one well-defined
    /// place relative to the directory they invoked the CLI from.
    ///
    /// If no project `.aura/` exists, returns an empty checker (every
    /// local tool call will prompt). If the user later accepts an "always
    /// allow" rule, it is persisted to `<cwd>/.aura/permissions.json`,
    /// creating the directory as needed.
    pub fn load(cwd: &Path) -> Result<Self> {
        Self::load_with_home(cwd, dirs::home_dir().as_deref())
    }

    /// Same as [`PermissionChecker::load`] but with an injectable home
    /// directory, so tests aren't affected by the developer's real `$HOME`.
    pub fn load_with_home(cwd: &Path, home: Option<&Path>) -> Result<Self> {
        let project_aura = find_project_aura_dir_with_home(cwd, home);

        // `save_dir` is the directory whose `.aura/` will hold persisted
        // rules. If we found an existing `.aura/`, use its parent (the
        // project root); otherwise default to cwd so a brand-new install
        // creates `<cwd>/.aura/permissions.json` on first save.
        let save_dir = project_aura
            .as_ref()
            .and_then(|d| d.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| cwd.to_path_buf());

        let Some(aura_dir) = project_aura else {
            return Ok(Self {
                save_dir,
                ..Self::default()
            });
        };

        let primary = aura_dir.join(PERMISSIONS_FILENAME);
        let legacy = aura_dir.join(LEGACY_PERMISSIONS_FILENAME);

        let path = if primary.is_file() {
            primary
        } else if legacy.is_file() {
            warn_legacy_permissions_once(&legacy);
            legacy
        } else {
            return Ok(Self {
                save_dir,
                ..Self::default()
            });
        };

        let contents = std::fs::read_to_string(&path)?;
        let settings: SettingsFile = serde_json::from_str(&contents)?;

        let allow = settings
            .permissions
            .allow
            .iter()
            .filter_map(|r| parse_rule(r))
            .collect();
        let deny = settings
            .permissions
            .deny
            .iter()
            .filter_map(|r| parse_rule(r))
            .collect();

        Ok(Self {
            allow,
            deny,
            save_dir,
            wildcard_hint_shown: HashSet::new(),
        })
    }

    /// Format the allow/deny rules as a human-readable string for the LLM.
    /// Returns `None` if no rules are configured (fully permissive).
    pub fn describe_rules(&self) -> Option<String> {
        if self.allow.is_empty() && self.deny.is_empty() {
            return None;
        }

        let mut parts = Vec::new();

        if !self.allow.is_empty() {
            let rules: Vec<String> = self
                .allow
                .iter()
                .map(|r| format!("{}({})", r.tool_name, r.argument_pattern))
                .collect();
            parts.push(format!("Allowed: {}", rules.join(", ")));
        }

        if !self.deny.is_empty() {
            let rules: Vec<String> = self
                .deny
                .iter()
                .map(|r| format!("{}({})", r.tool_name, r.argument_pattern))
                .collect();
            parts.push(format!("Denied: {}", rules.join(", ")));
        }

        Some(parts.join("\n"))
    }

    /// Check whether a tool call is permitted.
    /// Returns `Allowed` if an explicit allow rule matched,
    /// `Denied(reason)` if a deny rule matched, or
    /// `Prompt` if no rule matched and the user should be asked.
    /// Server-side tools (not local) are always allowed — permissions
    /// only apply to tools executed on the client machine.
    pub fn check(&self, tool_name: &str, arguments: &str) -> PermissionResult {
        if !crate::tools::is_local_tool(tool_name) {
            return PermissionResult::Allowed;
        }

        let primary_arg = extract_primary_argument(tool_name, arguments);

        // 1. Deny checked first
        for rule in &self.deny {
            if rule.tool_name == tool_name && glob_match(&rule.argument_pattern, &primary_arg) {
                return PermissionResult::Denied(format!(
                    "{tool_name} blocked by deny rule: {}({})",
                    rule.tool_name, rule.argument_pattern
                ));
            }
        }

        // 2. If allow list has a matching rule, permit immediately
        let matched = self.allow.iter().any(|rule| {
            rule.tool_name == tool_name && glob_match(&rule.argument_pattern, &primary_arg)
        });
        if matched {
            return PermissionResult::Allowed;
        }

        // 3. No matching rule — prompt the user
        PermissionResult::Prompt
    }

    /// If a tool already has many path-specific allow rules, print a one-time
    /// hint suggesting the user edit `.aura/permissions.json` to use a wildcard.
    fn maybe_show_wildcard_hint(&mut self, tool_name: &str) {
        if self.wildcard_hint_shown.contains(tool_name) {
            return;
        }

        // Count non-wildcard allow rules for this tool
        let specific_count = self
            .allow
            .iter()
            .filter(|r| r.tool_name == tool_name && r.argument_pattern != "*")
            .count();

        if specific_count >= WILDCARD_HINT_THRESHOLD {
            self.wildcard_hint_shown.insert(tool_name.to_string());
            eprintln!(
                "{}",
                format!(
                    "  Tip: You have {specific_count} path-specific {tool_name} rules in \
                     .aura/permissions.json. Consider editing the file to use a wildcard \
                     pattern like \"{tool_name}(./src/*)\" for broader access."
                )
                .with(crossterm::style::Color::DarkGrey),
            );
        }
    }

    /// Add a runtime allow rule (from an interactive "always allow" choice).
    pub fn add_allow_rule(&mut self, tool_name: &str, argument_pattern: &str) {
        self.allow.push(PermissionRule {
            tool_name: tool_name.to_string(),
            argument_pattern: argument_pattern.to_string(),
        });
    }

    /// Interactively prompt the user to allow or deny a tool call.
    ///
    /// Temporarily restores canonical terminal mode so the user can type.
    /// Returns `true` if the user allowed the call, `false` if denied.
    ///
    /// Options:
    ///   y — allow this one call
    ///   n — deny this one call
    ///   a — always allow this tool for this specific path/argument
    ///   d — always deny this tool (all arguments)
    pub fn prompt_tool_permission(&mut self, tool_name: &str, arguments: &str) -> bool {
        let primary_arg = extract_primary_argument(tool_name, arguments);

        // One-time hint: if there are many path-specific allow rules for this
        // tool, suggest the user consolidate with a wildcard pattern.
        self.maybe_show_wildcard_hint(tool_name);

        eprint!(
            "{}  {} ",
            format!("  Allow {tool_name}?").with(crossterm::style::Color::Yellow),
            "[y]es / [n]o / [a]lways allow / [d]eny always".with(crossterm::style::Color::DarkGrey),
        );
        let _ = io::stderr().flush();

        // Read a single valid keypress (non-canonical, no-echo, blocking).
        let choice = read_single_key(&['y', 'n', 'a', 'd']);

        let label = match choice {
            'y' => "[y]es",
            'n' => "[n]o",
            'a' => "[a]lways allow",
            'd' => "[d]eny always",
            _ => "",
        };

        // Replace the question line with a statement of what was chosen.
        let _ = crossterm::execute!(
            io::stderr(),
            crossterm::cursor::MoveToColumn(0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine),
        );
        eprintln!(
            "{} {}",
            format!("  Allow {tool_name}:").with(crossterm::style::Color::Yellow),
            label.with(crossterm::style::Color::White),
        );

        // For allow rules, scope to the exact path/argument.
        // For deny rules, block the entire tool.
        let allow_pattern = if primary_arg.is_empty() {
            "*".to_string()
        } else {
            primary_arg
        };

        let result = match choice {
            'y' => true,
            'a' => {
                self.add_allow_rule(tool_name, &allow_pattern);
                eprintln!(
                    "{}",
                    format!("  ✓ {tool_name}({allow_pattern}) allowed for this session.")
                        .with(crossterm::style::Color::Green),
                );

                if let Err(e) = self.persist_rule("allow", tool_name, &allow_pattern) {
                    eprintln!(
                        "{}",
                        format!("  Warning: could not save to permissions.json: {e}")
                            .with(crossterm::style::Color::Yellow),
                    );
                } else {
                    eprintln!(
                        "{}",
                        format!(
                            "  ✓ Saved {tool_name}({allow_pattern}) to .aura/permissions.json — \
                             you won't be asked again for this path."
                        )
                        .with(crossterm::style::Color::Green),
                    );
                }

                true
            }
            'd' => {
                self.add_deny_rule(tool_name, "*");
                eprintln!(
                    "{}",
                    format!("  ✗ {tool_name}(*) denied for this session.")
                        .with(crossterm::style::Color::Red),
                );

                if let Err(e) = self.persist_rule("deny", tool_name, "*") {
                    eprintln!(
                        "{}",
                        format!("  Warning: could not save to permissions.json: {e}")
                            .with(crossterm::style::Color::Yellow),
                    );
                } else {
                    eprintln!(
                        "{}",
                        format!(
                            "  ✗ Saved {tool_name}(*) deny to .aura/permissions.json — \
                             this tool will always be blocked."
                        )
                        .with(crossterm::style::Color::Red),
                    );
                }

                false
            }
            // 'n' — one-time deny (only valid key left)
            _ => false,
        };

        // Trailing blank line separates the permission block from what comes next.
        eprintln!();
        result
    }

    /// Add a runtime deny rule (from an interactive "deny always" choice).
    pub fn add_deny_rule(&mut self, tool_name: &str, argument_pattern: &str) {
        self.deny.push(PermissionRule {
            tool_name: tool_name.to_string(),
            argument_pattern: argument_pattern.to_string(),
        });
    }

    /// Persist a permission rule to `.aura/permissions.json` under
    /// `save_dir`. If a legacy `.aura/settings.json` exists in the same
    /// directory, its contents are migrated forward (the new rule is added
    /// alongside) and the legacy file is left in place — readers prefer
    /// `permissions.json` so the legacy copy is harmless and a manual
    /// rollback stays possible.
    ///
    /// `kind` must be `"allow"` or `"deny"`.
    fn persist_rule(&self, kind: &str, tool_name: &str, pattern: &str) -> Result<()> {
        let aura_dir = self.save_dir.join(".aura");
        if !aura_dir.exists() {
            std::fs::create_dir_all(&aura_dir)?;
        }

        let target_path = aura_dir.join(PERMISSIONS_FILENAME);
        let legacy_path = aura_dir.join(LEGACY_PERMISSIONS_FILENAME);

        // Seed from the new file if it exists, else from the legacy file
        // so we don't lose rules on the first save after upgrade.
        let mut root: serde_json::Value = if target_path.exists() {
            let contents = std::fs::read_to_string(&target_path)?;
            serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}))
        } else if legacy_path.exists() {
            let contents = std::fs::read_to_string(&legacy_path)?;
            serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        let rule_str = format!("{tool_name}({pattern})");

        let perms = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("permissions root is not a JSON object"))?
            .entry("permissions")
            .or_insert_with(|| serde_json::json!({}));
        let arr = perms
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("permissions is not a JSON object"))?
            .entry(kind)
            .or_insert_with(|| serde_json::json!([]));
        let arr = arr
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("permissions.{kind} is not a JSON array"))?;

        if !arr.iter().any(|v| v.as_str() == Some(&rule_str)) {
            arr.push(serde_json::Value::String(rule_str));
        }

        let formatted = serde_json::to_string_pretty(&root)?;
        std::fs::write(&target_path, formatted)?;
        Ok(())
    }
}

/// Warn once per process that the legacy `.aura/settings.json` filename is
/// being read, and point the user at the new name.
fn warn_legacy_permissions_once(path: &Path) {
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.set(()).is_err() {
        return;
    }

    eprintln!(
        "warning: {} is deprecated; rename to {} (the old name was \
         ambiguous now that the CLI also has a `cli.toml`).",
        path.display(),
        path.with_file_name(PERMISSIONS_FILENAME).display(),
    );
}

/// Read a single valid keypress from stdin, ignoring anything not in `valid`.
/// Puts the terminal into non-canonical, no-echo, **blocking** mode (VMIN=1)
/// for the read, then restores the normal non-blocking non-canonical mode
/// used during processing.
#[cfg(unix)]
fn read_single_key(valid: &[char]) -> char {
    unsafe {
        // Save current termios, then switch to blocking single-char reads.
        let mut saved: libc::termios = std::mem::zeroed();
        libc::tcgetattr(libc::STDIN_FILENO, &mut saved);

        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1; // block until 1 byte available
        raw.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);

        let result;
        loop {
            let mut byte = [0u8; 1];
            let n = libc::read(
                libc::STDIN_FILENO,
                byte.as_mut_ptr() as *mut libc::c_void,
                1,
            );
            if n == 1 {
                let ch = (byte[0] as char).to_ascii_lowercase();
                if valid.contains(&ch) {
                    result = ch;
                    break;
                }
                // Not a valid key — silently ignore, keep waiting.
            }
        }

        // Restore the previous terminal state.
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
        result
    }
}

#[cfg(not(unix))]
fn read_single_key(valid: &[char]) -> char {
    // Fallback for non-unix: line-buffered read, pick first valid char.
    loop {
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_ok() {
            if let Some(ch) = input.trim().chars().next() {
                let ch = ch.to_ascii_lowercase();
                if valid.contains(&ch) {
                    return ch;
                }
            }
        }
    }
}

/// Parse a rule string like `"Shell(ls *)"` into a `PermissionRule`.
fn parse_rule(rule: &str) -> Option<PermissionRule> {
    let open = rule.find('(')?;
    let close = rule.rfind(')')?;
    if close <= open + 1 {
        return None;
    }
    let tool_name = rule[..open].trim().to_string();
    if tool_name.is_empty() {
        return None;
    }
    let argument_pattern = rule[open + 1..close].trim().to_string();
    Some(PermissionRule {
        tool_name,
        argument_pattern,
    })
}

/// Extract the primary argument from a tool's JSON arguments string.
fn extract_primary_argument(tool_name: &str, arguments: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    match tool_name {
        "Shell" => args["command"].as_str().unwrap_or("").to_string(),
        "Read" => args["file_path"].as_str().unwrap_or("").to_string(),
        "ListFiles" | "FindFiles" | "SearchFiles" | "FileInfo" => {
            args["path"].as_str().unwrap_or("").to_string()
        }
        "Update" => args["file_path"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// Simple glob matching where `*` matches any sequence of characters.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();

    // No wildcard — exact match
    if segments.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0;

    // First segment must anchor at the start
    if let Some(first) = segments.first()
        && !first.is_empty()
    {
        if !text.starts_with(*first) {
            return false;
        }
        pos = first.len();
    }

    // Last segment must anchor at the end
    if let Some(last) = segments.last()
        && !last.is_empty()
        && !text.ends_with(*last)
    {
        return false;
    }

    // Middle segments must appear in order
    for (i, segment) in segments.iter().enumerate() {
        if i == 0 || i == segments.len() - 1 {
            continue;
        }
        if segment.is_empty() {
            continue;
        }
        if let Some(found) = text[pos..].find(*segment) {
            pos += found + segment.len();
        } else {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- glob_match tests ---

    #[test]
    fn glob_star_matches_everything() {
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn glob_prefix_star() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "foo/bar.rs"));
        assert!(!glob_match("*.rs", "main.txt"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(glob_match("ls *", "ls -la"));
        assert!(glob_match("ls *", "ls "));
        assert!(!glob_match("ls *", "cat foo"));
    }

    #[test]
    fn glob_middle_star() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "lib/main.rs"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("*src*rs*", "my/src/main.rs!"));
        assert!(glob_match("*src*rs", "src/main.rs"));
    }

    // --- parse_rule tests ---

    #[test]
    fn parse_rule_valid() {
        let rule = parse_rule("Shell(ls *)").unwrap();
        assert_eq!(rule.tool_name, "Shell");
        assert_eq!(rule.argument_pattern, "ls *");
    }

    #[test]
    fn parse_rule_wildcard() {
        let rule = parse_rule("Read(*)").unwrap();
        assert_eq!(rule.tool_name, "Read");
        assert_eq!(rule.argument_pattern, "*");
    }

    #[test]
    fn parse_rule_empty_tool_rejected() {
        assert!(parse_rule("(*)").is_none());
    }

    #[test]
    fn parse_rule_empty_parens_rejected() {
        assert!(parse_rule("Shell()").is_none());
    }

    #[test]
    fn parse_rule_no_parens_rejected() {
        assert!(parse_rule("Shell").is_none());
    }

    // --- PermissionChecker.check tests ---

    /// Helper to build a checker with rules for testing.
    fn test_checker(allow: Vec<PermissionRule>, deny: Vec<PermissionRule>) -> PermissionChecker {
        PermissionChecker {
            allow,
            deny,
            save_dir: PathBuf::from("/tmp/test"),
            wildcard_hint_shown: HashSet::new(),
        }
    }

    #[test]
    fn default_checker_prompts() {
        let checker = PermissionChecker::default();
        // No rules → should prompt for local tools
        assert_eq!(
            checker.check("Shell", r#"{"command":"ls"}"#),
            PermissionResult::Prompt,
        );
        assert_eq!(
            checker.check("Read", r#"{"file_path":"foo.rs"}"#),
            PermissionResult::Prompt,
        );
    }

    #[test]
    fn deny_blocks_matching_tool() {
        let checker = test_checker(
            vec![],
            vec![PermissionRule {
                tool_name: "Shell".to_string(),
                argument_pattern: "*".to_string(),
            }],
        );
        assert!(matches!(
            checker.check("Shell", r#"{"command":"ls"}"#),
            PermissionResult::Denied(_)
        ));
        // Read has no deny rule → prompt
        assert_eq!(
            checker.check("Read", r#"{"file_path":"foo.rs"}"#),
            PermissionResult::Prompt,
        );
    }

    #[test]
    fn allow_gates_access() {
        let checker = test_checker(
            vec![PermissionRule {
                tool_name: "Read".to_string(),
                argument_pattern: "*.rs".to_string(),
            }],
            vec![],
        );
        assert_eq!(
            checker.check("Read", r#"{"file_path":"main.rs"}"#),
            PermissionResult::Allowed,
        );
        // .txt not matched by allow rule → prompt
        assert_eq!(
            checker.check("Read", r#"{"file_path":"main.txt"}"#),
            PermissionResult::Prompt,
        );
        // Shell not in allow list → prompt
        assert_eq!(
            checker.check("Shell", r#"{"command":"ls"}"#),
            PermissionResult::Prompt,
        );
    }

    #[test]
    fn server_side_tools_always_allowed() {
        // Even with no allow rules, server-side tools pass through
        let checker = test_checker(
            vec![PermissionRule {
                tool_name: "Read".to_string(),
                argument_pattern: "*.rs".to_string(),
            }],
            vec![],
        );
        assert_eq!(
            checker.check("list_pipelines", r#"{}"#),
            PermissionResult::Allowed,
        );
        assert_eq!(
            checker.check("vector_search_docs", r#"{"query":"test"}"#),
            PermissionResult::Allowed,
        );
    }

    #[test]
    fn deny_overrides_allow() {
        let checker = test_checker(
            vec![PermissionRule {
                tool_name: "Shell".to_string(),
                argument_pattern: "*".to_string(),
            }],
            vec![PermissionRule {
                tool_name: "Shell".to_string(),
                argument_pattern: "rm *".to_string(),
            }],
        );
        assert_eq!(
            checker.check("Shell", r#"{"command":"ls"}"#),
            PermissionResult::Allowed,
        );
        assert!(matches!(
            checker.check("Shell", r#"{"command":"rm -rf /"}"#),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn extract_primary_argument_works() {
        assert_eq!(
            extract_primary_argument("Shell", r#"{"command":"echo hi"}"#),
            "echo hi"
        );
        assert_eq!(
            extract_primary_argument("Read", r#"{"file_path":"/tmp/foo.txt"}"#),
            "/tmp/foo.txt"
        );
        assert_eq!(
            extract_primary_argument("ListFiles", r#"{"path":"/tmp"}"#),
            "/tmp"
        );
        assert_eq!(
            extract_primary_argument("Unknown", r#"{"anything":"value"}"#),
            ""
        );
    }

    #[test]
    fn list_files_permission_check() {
        let checker = test_checker(
            vec![PermissionRule {
                tool_name: "ListFiles".to_string(),
                argument_pattern: "/home/*".to_string(),
            }],
            vec![],
        );
        assert_eq!(
            checker.check("ListFiles", r#"{"path":"/home/user"}"#),
            PermissionResult::Allowed,
        );
        // /etc not matched by allow rule → prompt
        assert_eq!(
            checker.check("ListFiles", r#"{"path":"/etc"}"#),
            PermissionResult::Prompt,
        );
    }

    // --- load() walk-up + migration tests ---

    /// Use an empty fake `$HOME` so the walk-up never accidentally treats
    /// the developer's real home as a project root. Returns the path to
    /// substitute for `$HOME` during the test.
    fn empty_fake_home() -> TempDir {
        TempDir::new().unwrap()
    }

    fn write_permissions_json(aura_dir: &Path, allow: &[&str], deny: &[&str]) {
        fs::create_dir_all(aura_dir).unwrap();
        let body = serde_json::json!({
            "permissions": { "allow": allow, "deny": deny }
        });
        fs::write(
            aura_dir.join(PERMISSIONS_FILENAME),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    fn write_legacy_settings_json(aura_dir: &Path, allow: &[&str], deny: &[&str]) {
        fs::create_dir_all(aura_dir).unwrap();
        let body = serde_json::json!({
            "permissions": { "allow": allow, "deny": deny }
        });
        fs::write(
            aura_dir.join(LEGACY_PERMISSIONS_FILENAME),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn load_finds_permissions_in_cwd() {
        let cwd = TempDir::new().unwrap();
        let home = empty_fake_home();
        write_permissions_json(&cwd.path().join(".aura"), &["Read(*.rs)"], &[]);

        let checker = PermissionChecker::load_with_home(cwd.path(), Some(home.path())).unwrap();
        assert_eq!(
            checker.check("Read", r#"{"file_path":"main.rs"}"#),
            PermissionResult::Allowed,
        );
    }

    #[test]
    fn load_walks_up_to_project_root_from_deep_subdir() {
        let project = TempDir::new().unwrap();
        let home = empty_fake_home();
        write_permissions_json(&project.path().join(".aura"), &["Shell(git status)"], &[]);

        let deep = project.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();

        let checker = PermissionChecker::load_with_home(&deep, Some(home.path())).unwrap();
        assert_eq!(
            checker.check("Shell", r#"{"command":"git status"}"#),
            PermissionResult::Allowed,
        );
    }

    #[test]
    fn load_returns_empty_when_no_aura_dir_anywhere_above() {
        // $HOME is some unrelated path. cwd is in a temp tree with no `.aura/`.
        let cwd = TempDir::new().unwrap();
        let home = empty_fake_home();

        let checker = PermissionChecker::load_with_home(cwd.path(), Some(home.path())).unwrap();
        // No rules → every local tool prompts.
        assert_eq!(
            checker.check("Shell", r#"{"command":"ls"}"#),
            PermissionResult::Prompt,
        );
    }

    #[test]
    fn load_skips_home_aura_when_cwd_is_home() {
        // If cwd happens to equal $HOME and $HOME has a .aura/permissions.json,
        // it must NOT be picked up — there is no global permissions concept.
        let home = TempDir::new().unwrap();
        write_permissions_json(&home.path().join(".aura"), &["Shell(*)"], &[]);

        let checker = PermissionChecker::load_with_home(home.path(), Some(home.path())).unwrap();
        // Should fall through to default (Prompt), not Allowed.
        assert_eq!(
            checker.check("Shell", r#"{"command":"ls"}"#),
            PermissionResult::Prompt,
        );
    }

    #[test]
    fn load_falls_back_to_legacy_settings_json() {
        let cwd = TempDir::new().unwrap();
        let home = empty_fake_home();
        // Only legacy settings.json exists — should still be honored.
        write_legacy_settings_json(&cwd.path().join(".aura"), &["Read(*.rs)"], &[]);

        let checker = PermissionChecker::load_with_home(cwd.path(), Some(home.path())).unwrap();
        assert_eq!(
            checker.check("Read", r#"{"file_path":"main.rs"}"#),
            PermissionResult::Allowed,
        );
    }

    #[test]
    fn load_prefers_permissions_json_over_legacy_settings_json() {
        let cwd = TempDir::new().unwrap();
        let home = empty_fake_home();
        let aura = cwd.path().join(".aura");
        // Both files exist with different rules — new name must win.
        write_permissions_json(&aura, &["Read(*.rs)"], &[]);
        write_legacy_settings_json(&aura, &["Read(*.txt)"], &[]);

        let checker = PermissionChecker::load_with_home(cwd.path(), Some(home.path())).unwrap();
        assert_eq!(
            checker.check("Read", r#"{"file_path":"main.rs"}"#),
            PermissionResult::Allowed,
        );
        assert_eq!(
            checker.check("Read", r#"{"file_path":"main.txt"}"#),
            PermissionResult::Prompt,
        );
    }

    #[test]
    fn persist_rule_writes_permissions_json_in_save_dir() {
        let project = TempDir::new().unwrap();
        let home = empty_fake_home();
        // No .aura/ yet. First "always allow" should create it.
        let checker = PermissionChecker::load_with_home(project.path(), Some(home.path())).unwrap();
        checker.persist_rule("allow", "Read", "*.rs").unwrap();

        let new_path = project.path().join(".aura").join(PERMISSIONS_FILENAME);
        assert!(
            new_path.is_file(),
            "expected new permissions.json to be written"
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&new_path).unwrap()).unwrap();
        let allow = parsed["permissions"]["allow"].as_array().unwrap();
        assert!(allow.iter().any(|v| v.as_str() == Some("Read(*.rs)")));
    }

    #[test]
    fn persist_rule_migrates_forward_from_legacy_settings_json() {
        let project = TempDir::new().unwrap();
        let home = empty_fake_home();
        let aura = project.path().join(".aura");
        // Pre-existing legacy file with one rule. New rule lands in
        // permissions.json alongside the migrated one.
        write_legacy_settings_json(&aura, &["Read(*.rs)"], &[]);

        let checker = PermissionChecker::load_with_home(project.path(), Some(home.path())).unwrap();
        checker.persist_rule("allow", "Shell", "ls").unwrap();

        let new_path = aura.join(PERMISSIONS_FILENAME);
        assert!(new_path.is_file());
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&new_path).unwrap()).unwrap();
        let allow = parsed["permissions"]["allow"].as_array().unwrap();
        let strs: Vec<&str> = allow.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            strs.contains(&"Read(*.rs)"),
            "legacy rule must carry forward"
        );
        assert!(strs.contains(&"Shell(ls)"), "new rule must be appended");
    }

    #[test]
    fn persist_rule_writes_to_discovered_root_not_cwd_when_invoked_from_subdir() {
        // User has perms at /project/.aura/, runs CLI from /project/sub/dir,
        // accepts an "always allow" — the new rule must land in
        // /project/.aura/permissions.json, NOT pollute /project/sub/dir.
        let project = TempDir::new().unwrap();
        let home = empty_fake_home();
        let project_aura = project.path().join(".aura");
        write_permissions_json(&project_aura, &["Read(*.rs)"], &[]);

        let deep = project.path().join("sub").join("dir");
        fs::create_dir_all(&deep).unwrap();

        let checker = PermissionChecker::load_with_home(&deep, Some(home.path())).unwrap();
        checker.persist_rule("allow", "Shell", "ls").unwrap();

        // Rule lands in the discovered .aura/, not in <deep>/.aura/
        let written = project_aura.join(PERMISSIONS_FILENAME);
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&written).unwrap()).unwrap();
        let allow = parsed["permissions"]["allow"].as_array().unwrap();
        let strs: Vec<&str> = allow.iter().filter_map(|v| v.as_str()).collect();
        assert!(strs.contains(&"Shell(ls)"));
        // Make sure we didn't create a stray .aura/ in the subdir.
        assert!(!deep.join(".aura").exists());
    }
}
