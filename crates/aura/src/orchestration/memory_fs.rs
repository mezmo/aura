use regex::RegexBuilder;
use serde::Serialize;
use std::path::{Component, Path, PathBuf};
use tokio::fs;

#[derive(Debug, Clone)]
pub struct MemoryFs {
    root: PathBuf,
    max_read_bytes: usize,
    max_search_results: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryFsOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub cwd: String,
    pub truncated: bool,
}

impl MemoryFsOutput {
    fn ok(stdout: String, cwd: String, truncated: bool) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            cwd,
            truncated,
        }
    }

    fn err(message: impl Into<String>, cwd: String) -> Self {
        Self {
            stdout: String::new(),
            stderr: message.into(),
            exit_code: 1,
            cwd,
            truncated: false,
        }
    }
}

impl MemoryFs {
    pub fn new(root: impl AsRef<Path>, max_read_bytes: usize, max_search_results: usize) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            max_read_bytes,
            max_search_results,
        }
    }

    pub async fn execute(
        &self,
        command: &str,
        cwd: Option<&str>,
    ) -> std::io::Result<MemoryFsOutput> {
        let cwd = normalize_virtual(cwd.unwrap_or("/")).unwrap_or_else(|| "/".to_string());

        if has_forbidden_shell_syntax(command) {
            return Ok(MemoryFsOutput::err(
                "unsupported shell syntax in memory command",
                cwd,
            ));
        }

        let tokens = match split_command(command) {
            Ok(tokens) if !tokens.is_empty() => tokens,
            Ok(_) => return Ok(MemoryFsOutput::ok(String::new(), cwd, false)),
            Err(e) => return Ok(MemoryFsOutput::err(e, cwd)),
        };

        match tokens[0].as_str() {
            "pwd" => Ok(MemoryFsOutput::ok(format!("{cwd}\n"), cwd, false)),
            "cat" => self.cat(&tokens[1..], &cwd).await,
            "head" => self.head_tail(&tokens[1..], &cwd, true).await,
            "tail" => self.head_tail(&tokens[1..], &cwd, false).await,
            "ls" => self.ls(&tokens[1..], &cwd).await,
            "stat" => self.stat(&tokens[1..], &cwd).await,
            "find" => self.find(&tokens[1..], &cwd).await,
            "grep" => self.grep(&tokens[1..], &cwd).await,
            "query" => self.query(&tokens[1..], &cwd).await,
            other => Ok(MemoryFsOutput::err(
                format!("unsupported memory command: {other}"),
                cwd,
            )),
        }
    }

    pub async fn read_path(
        &self,
        path: &str,
        cwd: Option<&str>,
    ) -> std::io::Result<MemoryFsOutput> {
        let cwd = normalize_virtual(cwd.unwrap_or("/")).unwrap_or_else(|| "/".to_string());
        self.read_file(path, &cwd).await
    }

    pub async fn search_path(
        &self,
        path: &str,
        pattern: &str,
        cwd: Option<&str>,
        case_sensitive: bool,
        regex: bool,
    ) -> std::io::Result<MemoryFsOutput> {
        let cwd = normalize_virtual(cwd.unwrap_or("/")).unwrap_or_else(|| "/".to_string());
        self.search(path, pattern, &cwd, case_sensitive, !regex, true)
            .await
    }

    pub async fn find_path(
        &self,
        path: &str,
        name_pattern: Option<&str>,
        cwd: Option<&str>,
    ) -> std::io::Result<MemoryFsOutput> {
        let cwd = normalize_virtual(cwd.unwrap_or("/")).unwrap_or_else(|| "/".to_string());
        let mut args = vec![path.to_string()];
        if let Some(pattern) = name_pattern {
            args.push("-name".to_string());
            args.push(pattern.to_string());
        }
        self.find(&args, &cwd).await
    }

    async fn cat(&self, args: &[String], cwd: &str) -> std::io::Result<MemoryFsOutput> {
        if args.len() != 1 {
            return Ok(MemoryFsOutput::err(
                "cat requires one path",
                cwd.to_string(),
            ));
        }
        self.read_file(&args[0], cwd).await
    }

    async fn head_tail(
        &self,
        args: &[String],
        cwd: &str,
        is_head: bool,
    ) -> std::io::Result<MemoryFsOutput> {
        let mut n = 10usize;
        let mut path = None;
        let mut i = 0usize;
        while i < args.len() {
            match args[i].as_str() {
                "-n" => {
                    i += 1;
                    if i >= args.len() {
                        return Ok(MemoryFsOutput::err("missing -n value", cwd.to_string()));
                    }
                    n = args[i].parse().unwrap_or(10);
                }
                value => path = Some(value.to_string()),
            }
            i += 1;
        }

        let Some(path) = path else {
            return Ok(MemoryFsOutput::err("missing path", cwd.to_string()));
        };
        let output = self.read_file(&path, cwd).await?;
        if output.exit_code != 0 {
            return Ok(output);
        }
        let lines: Vec<&str> = output.stdout.lines().collect();
        let selected: Vec<&str> = if is_head {
            lines.into_iter().take(n).collect()
        } else {
            lines
                .into_iter()
                .rev()
                .take(n)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        };
        Ok(MemoryFsOutput::ok(
            format!("{}\n", selected.join("\n")),
            cwd.to_string(),
            output.truncated,
        ))
    }

    async fn ls(&self, args: &[String], cwd: &str) -> std::io::Result<MemoryFsOutput> {
        let path = args.first().map(String::as_str).unwrap_or(".");
        let resolved = match self.resolve(path, cwd) {
            Ok(path) => path,
            Err(e) => return Ok(MemoryFsOutput::err(e, cwd.to_string())),
        };
        let mut entries = fs::read_dir(&resolved.real).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().await?;
            let suffix = if meta.is_dir() { "/" } else { "" };
            names.push(format!("{name}{suffix}"));
        }
        names.sort();
        Ok(MemoryFsOutput::ok(
            if names.is_empty() {
                String::new()
            } else {
                format!("{}\n", names.join("\n"))
            },
            cwd.to_string(),
            false,
        ))
    }

    async fn stat(&self, args: &[String], cwd: &str) -> std::io::Result<MemoryFsOutput> {
        if args.len() != 1 {
            return Ok(MemoryFsOutput::err(
                "stat requires one path",
                cwd.to_string(),
            ));
        }
        let resolved = match self.resolve(&args[0], cwd) {
            Ok(path) => path,
            Err(e) => return Ok(MemoryFsOutput::err(e, cwd.to_string())),
        };
        let meta = fs::metadata(&resolved.real).await?;
        let kind = if meta.is_dir() { "directory" } else { "file" };
        Ok(MemoryFsOutput::ok(
            format!(
                "path: {}\ntype: {kind}\nbytes: {}\n",
                resolved.virtual_path,
                meta.len()
            ),
            cwd.to_string(),
            false,
        ))
    }

    async fn find(&self, args: &[String], cwd: &str) -> std::io::Result<MemoryFsOutput> {
        if args.is_empty() {
            return Ok(MemoryFsOutput::err("find requires a path", cwd.to_string()));
        }
        let path = args[0].clone();
        let mut name_pattern: Option<String> = None;
        let mut i = 1usize;
        while i < args.len() {
            if args[i] == "-name" {
                i += 1;
                if i >= args.len() {
                    return Ok(MemoryFsOutput::err(
                        "missing -name pattern",
                        cwd.to_string(),
                    ));
                }
                name_pattern = Some(args[i].clone());
            }
            i += 1;
        }
        let resolved = match self.resolve(&path, cwd) {
            Ok(path) => path,
            Err(e) => return Ok(MemoryFsOutput::err(e, cwd.to_string())),
        };
        let mut out = Vec::new();
        self.walk_collect(
            &resolved.real,
            &resolved.virtual_path,
            &mut |real, virtual_path| {
                let matches = name_pattern
                    .as_ref()
                    .map(|p| {
                        glob_name_match(p, real.file_name().and_then(|n| n.to_str()).unwrap_or(""))
                    })
                    .unwrap_or(true);
                if matches {
                    out.push(virtual_path.to_string());
                }
                out.len() < self.max_search_results
            },
        )
        .await?;
        let truncated = out.len() >= self.max_search_results;
        Ok(MemoryFsOutput::ok(
            if out.is_empty() {
                String::new()
            } else {
                format!("{}\n", out.join("\n"))
            },
            cwd.to_string(),
            truncated,
        ))
    }

    async fn grep(&self, args: &[String], cwd: &str) -> std::io::Result<MemoryFsOutput> {
        let mut case_sensitive = true;
        let mut recursive = false;
        let mut fixed = false;
        let mut positional = Vec::new();
        for arg in args {
            match arg.as_str() {
                "-i" => case_sensitive = false,
                "-r" => recursive = true,
                "-ri" | "-ir" => {
                    case_sensitive = false;
                    recursive = true;
                }
                "--fixed-strings" => fixed = true,
                value => positional.push(value.to_string()),
            }
        }
        if positional.len() != 2 {
            return Ok(MemoryFsOutput::err(
                "grep requires a pattern and path",
                cwd.to_string(),
            ));
        }
        self.search(
            &positional[1],
            &positional[0],
            cwd,
            case_sensitive,
            fixed,
            recursive,
        )
        .await
    }

    async fn query(&self, args: &[String], cwd: &str) -> std::io::Result<MemoryFsOutput> {
        if args.len() != 5 || args[1] != "--field" || args[3] != "--equals" {
            return Ok(MemoryFsOutput::err(
                "query usage: query <path> --field FIELD --equals VALUE",
                cwd.to_string(),
            ));
        }
        let resolved = match self.resolve(&args[0], cwd) {
            Ok(path) => path,
            Err(e) => return Ok(MemoryFsOutput::err(e, cwd.to_string())),
        };
        let content = fs::read_to_string(&resolved.real).await?;
        let mut matches = Vec::new();
        for line in content.lines() {
            let candidate = if content.trim_start().starts_with('{') && content.lines().count() == 1
            {
                content.as_str()
            } else {
                line
            };
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate)
                && json_field_equals(&value, &args[2], &args[4])
            {
                matches.push(candidate.to_string());
            }
            if content.trim_start().starts_with('{') && content.lines().count() == 1 {
                break;
            }
        }
        Ok(MemoryFsOutput::ok(
            if matches.is_empty() {
                String::new()
            } else {
                format!("{}\n", matches.join("\n"))
            },
            cwd.to_string(),
            matches.len() >= self.max_search_results,
        ))
    }

    async fn read_file(&self, path: &str, cwd: &str) -> std::io::Result<MemoryFsOutput> {
        let resolved = match self.resolve(path, cwd) {
            Ok(path) => path,
            Err(e) => return Ok(MemoryFsOutput::err(e, cwd.to_string())),
        };
        let data = fs::read(&resolved.real).await?;
        let truncated = data.len() > self.max_read_bytes;
        let data = &data[..data.len().min(self.max_read_bytes)];
        let stdout = String::from_utf8_lossy(data).to_string();
        Ok(MemoryFsOutput::ok(stdout, cwd.to_string(), truncated))
    }

    async fn search(
        &self,
        path: &str,
        pattern: &str,
        cwd: &str,
        case_sensitive: bool,
        fixed: bool,
        recursive: bool,
    ) -> std::io::Result<MemoryFsOutput> {
        let resolved = match self.resolve(path, cwd) {
            Ok(path) => path,
            Err(e) => return Ok(MemoryFsOutput::err(e, cwd.to_string())),
        };
        let regex = if fixed {
            None
        } else {
            match RegexBuilder::new(pattern)
                .case_insensitive(!case_sensitive)
                .build()
            {
                Ok(r) => Some(r),
                Err(e) => {
                    return Ok(MemoryFsOutput::err(
                        format!("invalid regex: {e}"),
                        cwd.to_string(),
                    ));
                }
            }
        };
        let needle = if case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };
        let mut matches = Vec::new();
        let mut files = Vec::new();
        if resolved.real.is_file() {
            files.push((resolved.real, resolved.virtual_path));
        } else if recursive || resolved.real.is_dir() {
            self.walk_collect(
                &resolved.real,
                &resolved.virtual_path,
                &mut |real, virtual_path| {
                    if real.is_file() {
                        files.push((real.to_path_buf(), virtual_path.to_string()));
                    }
                    true
                },
            )
            .await?;
        } else {
            return Ok(MemoryFsOutput::err(
                "grep target is not a file",
                cwd.to_string(),
            ));
        }

        for (real, virtual_path) in files {
            if is_binary(&real).await {
                continue;
            }
            let Ok(content) = fs::read_to_string(&real).await else {
                continue;
            };
            for (idx, line) in content.lines().enumerate() {
                let found = if let Some(regex) = &regex {
                    regex.is_match(line)
                } else if case_sensitive {
                    line.contains(&needle)
                } else {
                    line.to_lowercase().contains(&needle)
                };
                if found {
                    matches.push(format!("{}:{}:{}", virtual_path, idx + 1, line));
                    if matches.len() >= self.max_search_results {
                        return Ok(MemoryFsOutput::ok(
                            format!("{}\n", matches.join("\n")),
                            cwd.to_string(),
                            true,
                        ));
                    }
                }
            }
        }
        Ok(MemoryFsOutput::ok(
            if matches.is_empty() {
                String::new()
            } else {
                format!("{}\n", matches.join("\n"))
            },
            cwd.to_string(),
            false,
        ))
    }

    async fn walk_collect<F>(
        &self,
        root: &Path,
        virtual_root: &str,
        visitor: &mut F,
    ) -> std::io::Result<()>
    where
        F: FnMut(&Path, &str) -> bool,
    {
        let mut stack = vec![(root.to_path_buf(), virtual_root.to_string())];
        while let Some((path, virtual_path)) = stack.pop() {
            if !visitor(&path, &virtual_path) {
                break;
            }
            if path.is_dir() {
                let mut entries = fs::read_dir(&path).await?;
                let mut children = Vec::new();
                while let Some(entry) = entries.next_entry().await? {
                    let child = entry.path();
                    let name = entry.file_name().to_string_lossy().to_string();
                    let child_virtual = join_virtual(&virtual_path, &name);
                    children.push((child, child_virtual));
                }
                children.sort_by(|a, b| b.1.cmp(&a.1));
                stack.extend(children);
            }
        }
        Ok(())
    }

    fn resolve(&self, path: &str, cwd: &str) -> Result<ResolvedPath, String> {
        let virtual_path = resolve_virtual(path, cwd)?;
        let relative = virtual_path.trim_start_matches('/');
        let real = self.root.join(relative);
        Ok(ResolvedPath { real, virtual_path })
    }
}

struct ResolvedPath {
    real: PathBuf,
    virtual_path: String,
}

fn has_forbidden_shell_syntax(command: &str) -> bool {
    ["|", ">", "<", ";", "$(", "`", "&&", "||"]
        .iter()
        .any(|marker| command.contains(marker))
}

fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for c in command.chars() {
        match c {
            '"' => in_quote = !in_quote,
            ' ' | '\t' if !in_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if in_quote {
        return Err("unterminated quote".to_string());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn normalize_virtual(path: &str) -> Option<String> {
    resolve_virtual(path, "/").ok()
}

fn resolve_virtual(path: &str, cwd: &str) -> Result<String, String> {
    let combined = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        PathBuf::from(cwd).join(path)
    };
    let mut parts: Vec<String> = Vec::new();
    for component in combined.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err("path resolves outside memory root".to_string());
                }
            }
            _ => return Err("unsupported memory path".to_string()),
        }
    }
    Ok(if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    })
}

fn join_virtual(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

fn glob_name_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}

async fn is_binary(path: &Path) -> bool {
    let Ok(data) = fs::read(path).await else {
        return true;
    };
    data.iter().take(8192).any(|b| *b == 0)
}

fn json_field_equals(value: &serde_json::Value, field: &str, expected: &str) -> bool {
    value
        .get(field)
        .map(|v| {
            v.as_str()
                .map(|s| s == expected)
                .unwrap_or_else(|| v.to_string().trim_matches('"') == expected)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &std::path::Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let fs = MemoryFs::new(tmp.path(), 1024, 10);

        let output = fs.execute("cat ../../../etc/passwd", None).await.unwrap();

        assert_ne!(output.exit_code, 0);
        assert!(output.stderr.contains("outside memory root"));
    }

    #[tokio::test]
    async fn supports_fixed_and_case_insensitive_grep() {
        let tmp = TempDir::new().unwrap();
        write_file(
            &tmp.path().join("memory/event-2026-04-23.md"),
            "Line 1\nAura uses Archil as a mounted filesystem.\nfoo.bar is literal\n",
        );
        let fs = MemoryFs::new(tmp.path(), 1024, 10);

        let fixed = fs
            .execute(r#"grep --fixed-strings "foo.bar" /memory"#, None)
            .await
            .unwrap();
        let insensitive = fs.execute("grep -ri archil /memory", None).await.unwrap();

        assert_eq!(fixed.exit_code, 0);
        assert!(fixed.stdout.contains("/memory/event-2026-04-23.md:3:"));
        assert_eq!(insensitive.exit_code, 0);
        assert!(insensitive.stdout.contains("Archil"));
    }

    #[tokio::test]
    async fn query_matches_top_level_json_fields() {
        let tmp = TempDir::new().unwrap();
        write_file(
            &tmp.path().join("cs_123/run_456/manifest.json"),
            r#"{"status":"success","goal":"remember archil"}"#,
        );
        let fs = MemoryFs::new(tmp.path(), 4096, 10);

        let output = fs
            .execute(
                "query /cs_123/run_456/manifest.json --field status --equals success",
                None,
            )
            .await
            .unwrap();

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("remember archil"));
    }

    #[tokio::test]
    async fn rejects_shell_syntax_and_write_commands() {
        let tmp = TempDir::new().unwrap();
        let fs = MemoryFs::new(tmp.path(), 1024, 10);

        for command in [
            "rm /memory/index.md",
            "cat /memory/index.md | head",
            "echo hi > x",
        ] {
            let output = fs.execute(command, None).await.unwrap();
            assert_ne!(output.exit_code, 0, "{command}");
        }
    }
}
