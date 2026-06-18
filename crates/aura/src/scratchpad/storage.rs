//! Scratchpad storage: file I/O, path validation, format detection, cleanup.

use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info, warn};

/// Detected format of scratchpad content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentFormat {
    Json,
    Markdown,
    Text,
}

impl ContentFormat {
    /// Detect format and return the parsed JSON value if valid.
    /// Avoids double-parsing when the caller needs both format and value.
    pub fn detect_and_parse(content: &str) -> (Self, Option<serde_json::Value>) {
        let trimmed = content.trim();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return (ContentFormat::Json, Some(value));
        }
        if Self::is_markdown(trimmed) {
            return (ContentFormat::Markdown, None);
        }
        (ContentFormat::Text, None)
    }

    /// Detect whether content is structured markdown.
    ///
    /// Heuristic: returns true when any of the first 50 lines is a valid
    /// CommonMark/GFM ATX header (`# `, `## `, ..., `###### ` — 1 to 6 `#`
    /// followed by a space or tab). Header presence alone is sufficient; a
    /// header-only document (no lists, tables, or code fences) is still
    /// markdown, and requiring co-occurring list bullets misses real
    /// markdown content for no good reason.
    ///
    /// The space-after-hash requirement is what excludes the false
    /// positives that *would* otherwise leak through a bare `starts_with('#')`
    /// check: shebangs (`#!/bin/bash`), C preprocessor (`#include`),
    /// Python/shell comments without a space (`#TODO`). Real ATX headers
    /// always have the separator, per CommonMark §4.2.
    pub fn is_markdown(content: &str) -> bool {
        content
            .lines()
            .take(50)
            .any(|line| is_atx_header(line.trim_start()))
    }

    pub fn extension(&self) -> &'static str {
        match self {
            ContentFormat::Json => "json",
            ContentFormat::Markdown => "md",
            ContentFormat::Text => "txt",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ContentFormat::Json => "json",
            ContentFormat::Markdown => "markdown",
            ContentFormat::Text => "text",
        }
    }
}

/// CommonMark ATX header check: 1–6 `#` characters followed by a space or
/// tab. The trailing-whitespace rule is the load-bearing part — it's what
/// distinguishes a real header from `#!` shebangs, `#include` directives,
/// and `#TODO`-style comment markers that share the leading hash.
fn is_atx_header(line: &str) -> bool {
    let bytes = line.as_bytes();
    let hash_count = bytes.iter().take_while(|&&b| b == b'#').count();
    (1..=6).contains(&hash_count)
        && bytes
            .get(hash_count)
            .is_some_and(|&b| b == b' ' || b == b'\t')
}

/// A companion file extracted from a JSON string value.
#[derive(Debug, Clone)]
pub struct CompanionFile {
    /// Filename of the companion file.
    pub filename: String,
    /// The JSON key this was extracted from.
    pub source_key: String,
    /// Detected format of the companion content.
    pub format: ContentFormat,
    /// Number of lines in the companion file.
    pub line_count: usize,
}

/// Result of writing a scratchpad file, including any extracted companion files.
#[derive(Debug, Clone)]
pub struct WriteResult {
    /// Path of the primary file.
    pub path: PathBuf,
    /// Detected format of the primary content.
    pub format: ContentFormat,
    /// Number of lines in the primary file.
    pub line_count: usize,
    /// Companion files extracted from JSON string values.
    pub companions: Vec<CompanionFile>,
}

/// Intermediate representation of a companion file to be written.
struct PendingCompanion {
    filename: String,
    content: String,
    companion: CompanionFile,
}

/// Minimum number of lines a JSON string value must have to be extracted
/// as a companion file.
const COMPANION_MIN_LINES: usize = 10;

/// Manages scratchpad file storage for a single request.
#[derive(Debug, Clone)]
pub struct ScratchpadStorage {
    /// Directory for this request's scratchpad files; all writes land here.
    dir: PathBuf,
    /// Boundary for reads (an ancestor of `dir`, or `dir` itself). `validate_path`
    /// permits reads anywhere under this root; writes stay confined to `dir`.
    read_root: PathBuf,
}

impl ScratchpadStorage {
    /// Create scratchpad storage under the given directory.
    ///
    /// Creates `{parent}/scratchpad/` for storing intercepted tool outputs.
    pub async fn in_dir(parent: &Path) -> std::io::Result<Self> {
        let dir = parent.join("scratchpad");
        fs::create_dir_all(&dir).await?;
        info!("Scratchpad directory created: {}", dir.display());
        Ok(Self {
            read_root: dir.clone(),
            dir,
        })
    }

    /// Create storage with a specific base directory (for testing).
    pub async fn with_base_dir(base: &Path, request_id: &str) -> std::io::Result<Self> {
        let dir = base.join(request_id);
        fs::create_dir_all(&dir).await?;
        Ok(Self {
            read_root: dir.clone(),
            dir,
        })
    }

    /// Widen the read boundary to `read_root` (must be an ancestor of `dir`).
    ///
    /// Reads via [`validate_path`](Self::validate_path) are then permitted
    /// anywhere under `read_root`, while writes stay confined to `dir`. If
    /// `read_root` is not an ancestor of `dir`, the call is a no-op (the read
    /// boundary stays at `dir`) — this keeps the invariant that the scratchpad
    /// write dir is always readable.
    pub fn with_read_root(mut self, read_root: PathBuf) -> Self {
        let normalized = normalize_path(&read_root);
        if self.dir.starts_with(&normalized) {
            self.read_root = normalized;
        } else {
            warn!(
                "Ignoring scratchpad read_root {} — not an ancestor of write dir {}",
                normalized.display(),
                self.dir.display()
            );
        }
        self
    }

    /// Get the scratchpad directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Get the read boundary (writes are confined to [`dir`](Self::dir), but
    /// reads are permitted anywhere under this root).
    pub fn read_root(&self) -> &Path {
        &self.read_root
    }

    /// Build a tool-usable `file=` token for an absolute path under the read
    /// root, expressed relative to the scratchpad write dir so it can be passed
    /// straight back to the read tools (`head`, `slice`, `grep`, `read`, …).
    ///
    /// For an artifact at `{run}/artifacts/x.txt` with the scratchpad dir at
    /// `{run}/iteration-1/scratchpad`, this returns `../../artifacts/x.txt`.
    /// The result is validated through [`validate_path`](Self::validate_path),
    /// so a path outside the read root is rejected.
    pub fn relative_ref(&self, abs: &Path) -> Result<String, ScratchpadPathError> {
        let abs_norm = normalize_path(abs);
        let dir_norm = normalize_path(&self.dir);

        // Both the target and the scratchpad dir must live under the read root.
        let target_rel = abs_norm.strip_prefix(&self.read_root).map_err(|_| {
            ScratchpadPathError::OutsideDirectory {
                path: abs.display().to_string(),
                dir: self.read_root.display().to_string(),
            }
        })?;
        let dir_rel = dir_norm.strip_prefix(&self.read_root).map_err(|_| {
            ScratchpadPathError::OutsideDirectory {
                path: self.dir.display().to_string(),
                dir: self.read_root.display().to_string(),
            }
        })?;

        let mut rel = PathBuf::new();
        for _ in dir_rel.components() {
            rel.push("..");
        }
        rel.push(target_rel);

        let token = rel.to_string_lossy().to_string();
        // Defense-in-depth: confirm the token resolves back inside the read root.
        self.validate_path(&token)?;
        Ok(token)
    }

    /// Prepare content for writing: detect format, pretty-print JSON, resolve path.
    /// Returns `(path, format, content_to_write, parsed_json)`.
    fn prepare_write(
        &self,
        tool_call_id: &str,
        content: &str,
    ) -> (PathBuf, ContentFormat, String, Option<serde_json::Value>) {
        let (format, parsed) = ContentFormat::detect_and_parse(content);
        let filename = format!("{tool_call_id}.{}", format.extension());
        let path = self.dir.join(&filename);
        let to_write = match &parsed {
            Some(value) => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| content.to_string())
            }
            None => content.to_string(),
        };
        (path, format, to_write, parsed)
    }

    /// Write tool output to a scratchpad file.
    ///
    /// JSON content is pretty-printed before writing. Large string values
    /// inside JSON that contain structured content (markdown, escaped JSON)
    /// are extracted to companion files for direct exploration with line-based tools.
    pub async fn write_output(
        &self,
        tool_call_id: &str,
        content: &str,
    ) -> std::io::Result<WriteResult> {
        let (path, format, to_write, parsed) = self.prepare_write(tool_call_id, content);
        let line_count = to_write.lines().count();
        fs::write(&path, &to_write).await?;
        debug!(
            "Scratchpad file written: {} ({} bytes, {} lines, {})",
            path.display(),
            to_write.len(),
            line_count,
            format.as_str()
        );

        let companions = if let Some(value) = &parsed {
            self.extract_companions_async(tool_call_id, value).await?
        } else {
            vec![]
        };

        Ok(WriteResult {
            path,
            format,
            line_count,
            companions,
        })
    }

    /// Write tool output synchronously (for use in sync callbacks like `transform_output`).
    ///
    /// JSON content is pretty-printed before writing. Large string values
    /// inside JSON that contain structured content (markdown, escaped JSON)
    /// are extracted to companion files for direct exploration with line-based tools.
    pub fn write_output_sync(
        &self,
        tool_call_id: &str,
        content: &str,
    ) -> std::io::Result<WriteResult> {
        let (path, format, to_write, parsed) = self.prepare_write(tool_call_id, content);
        let line_count = to_write.lines().count();
        std::fs::write(&path, &to_write)?;
        debug!(
            "Scratchpad file written: {} ({} bytes, {} lines, {})",
            path.display(),
            to_write.len(),
            line_count,
            format.as_str()
        );

        let companions = if let Some(value) = &parsed {
            self.extract_companions_sync(tool_call_id, value)?
        } else {
            vec![]
        };

        Ok(WriteResult {
            path,
            format,
            line_count,
            companions,
        })
    }

    /// Extract large structured string values from JSON as companion files (async).
    async fn extract_companions_async(
        &self,
        tool_call_id: &str,
        value: &serde_json::Value,
    ) -> std::io::Result<Vec<CompanionFile>> {
        let mut companions = Vec::new();
        for pending in Self::plan_companions(tool_call_id, value) {
            let path = self.safe_companion_path(&pending.filename)?;
            fs::write(&path, &pending.content).await?;
            debug!(
                "Companion file extracted: {} (key={}, {} lines, {})",
                path.display(),
                pending.companion.source_key,
                pending.companion.line_count,
                pending.companion.format.as_str()
            );
            companions.push(pending.companion);
        }
        Ok(companions)
    }

    /// Extract large structured string values from JSON as companion files (sync).
    fn extract_companions_sync(
        &self,
        tool_call_id: &str,
        value: &serde_json::Value,
    ) -> std::io::Result<Vec<CompanionFile>> {
        let mut companions = Vec::new();
        for pending in Self::plan_companions(tool_call_id, value) {
            let path = self.safe_companion_path(&pending.filename)?;
            std::fs::write(&path, &pending.content)?;
            debug!(
                "Companion file extracted: {} (key={}, {} lines, {})",
                path.display(),
                pending.companion.source_key,
                pending.companion.line_count,
                pending.companion.format.as_str()
            );
            companions.push(pending.companion);
        }
        Ok(companions)
    }

    /// Resolve a companion filename inside the scratchpad directory. Rejects
    /// any filename that would escape the dir (defense-in-depth behind the
    /// `slugify_component` applied upstream).
    fn safe_companion_path(&self, filename: &str) -> std::io::Result<PathBuf> {
        self.validate_path(filename).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Rejected companion filename {filename:?}: {e}"),
            )
        })
    }

    /// Determine which companion files to extract from a JSON value.
    ///
    /// Checks top-level string values for structured content:
    /// 1. Escaped JSON strings → extracted as pretty-printed `.json`
    /// 2. Markdown (headers + lists) → extracted as `.md`
    ///
    /// Plain text is intentionally not extracted — the existing exploration
    /// tools (`get_in` with offset/limit, `grep`) work fine on it through the
    /// parent JSON file.
    fn plan_companions(tool_call_id: &str, value: &serde_json::Value) -> Vec<PendingCompanion> {
        let obj = match value.as_object() {
            Some(obj) => obj,
            None => return vec![],
        };

        let mut pending = Vec::new();
        for (key, val) in obj {
            if let Some(s) = val.as_str()
                && let Some(p) = Self::plan_one_companion(tool_call_id, key, s)
            {
                pending.push(p);
            }
        }
        pending
    }

    /// Evaluate a single string value for companion extraction.
    fn plan_one_companion(tool_call_id: &str, key: &str, value: &str) -> Option<PendingCompanion> {
        let trimmed = value.trim();

        // 1. Try escaped JSON — parse and pretty-print
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            let pretty =
                serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| value.to_string());
            return Self::make_companion(tool_call_id, key, pretty, ContentFormat::Json);
        }

        // 2. Try markdown
        if ContentFormat::is_markdown(value) {
            return Self::make_companion(
                tool_call_id,
                key,
                value.to_string(),
                ContentFormat::Markdown,
            );
        }

        // Plain text strings are not extracted — the existing tools (get_in with
        // offset/limit, grep) work fine on them through the parent JSON file.
        None
    }

    /// Build a `PendingCompanion` if the content meets the minimum line threshold.
    fn make_companion(
        tool_call_id: &str,
        key: &str,
        content: String,
        format: ContentFormat,
    ) -> Option<PendingCompanion> {
        let line_count = content.lines().count();
        if line_count < COMPANION_MIN_LINES {
            return None;
        }
        // The key comes from untrusted tool output. Slugify to keep the
        // filename inside the scratchpad directory; `write_pending_companion`
        // re-validates with the storage's `validate_path` before writing.
        let slug_key = slugify_component(key);
        let filename = format!("{tool_call_id}.{slug_key}.{}", format.extension());
        Some(PendingCompanion {
            filename: filename.clone(),
            content,
            companion: CompanionFile {
                filename,
                source_key: key.to_string(),
                format,
                line_count,
            },
        })
    }

    /// Validate that a file path is within this scratchpad directory.
    ///
    /// Two-stage check, deepest-first:
    ///
    /// 1. **Lexical normalization**: resolve `.` / `..` segments and verify
    ///    the result is under `self.dir`. Catches `../../etc/passwd`-style
    ///    traversal in the requested *string*.
    /// 2. **Symlink resolution** (if the file exists): canonicalize via
    ///    `std::fs::canonicalize`, follow symlinks, and re-verify that the
    ///    real path is under `self.dir`. Catches an attacker-planted symlink
    ///    inside the scratchpad dir that points outside.
    ///
    /// **Trust boundary**: this only matters if a process with write access
    /// to `memory_dir` is hostile. In the orchestration flow only aura
    /// itself writes to the scratchpad directory (via
    /// `ScratchpadStorage::write_output_sync`), and aura never creates
    /// symlinks. Stage 2 is therefore defense-in-depth against an
    /// out-of-band adversary (e.g., a multi-tenant host where another
    /// process can write to `/tmp`).
    ///
    /// For files that don't exist yet (e.g., the destination path during a
    /// write), stage 2 is skipped — `canonicalize` would fail with
    /// `ENOENT`. The lexical check still applies.
    pub fn validate_path(&self, file_path: &str) -> Result<PathBuf, ScratchpadPathError> {
        let requested = self.dir.join(file_path);

        // Stage 1: lexical normalization (handles ../).
        let normalized = normalize_path(&requested);

        if !normalized.starts_with(&self.read_root) {
            return Err(ScratchpadPathError::OutsideDirectory {
                path: file_path.to_string(),
                dir: self.read_root.display().to_string(),
            });
        }

        // Stage 2: symlink resolution if the file exists. We compare the
        // canonicalized request against the canonicalized read root
        // (canonicalize is consistent on macOS where /tmp is a symlink for
        // /private/tmp — comparing one canonical to one non-canonical
        // would always reject).
        if normalized.exists() {
            let canonical_root = std::fs::canonicalize(&self.read_root).map_err(|_| {
                ScratchpadPathError::OutsideDirectory {
                    path: file_path.to_string(),
                    dir: self.read_root.display().to_string(),
                }
            })?;
            let canonical_path = std::fs::canonicalize(&normalized).map_err(|_| {
                ScratchpadPathError::OutsideDirectory {
                    path: file_path.to_string(),
                    dir: self.read_root.display().to_string(),
                }
            })?;
            if !canonical_path.starts_with(&canonical_root) {
                return Err(ScratchpadPathError::OutsideDirectory {
                    path: file_path.to_string(),
                    dir: self.read_root.display().to_string(),
                });
            }
        }

        Ok(normalized)
    }

    /// List all scratchpad files in storage.
    pub async fn list_files(&self) -> std::io::Result<Vec<String>> {
        let mut files = Vec::new();
        let mut entries = fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.metadata().await?.is_file() {
                files.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        files.sort();
        Ok(files)
    }

    /// Find companion files for a primary scratchpad file. Companions are
    /// written by `extract_companions_*` and follow the naming convention
    /// `{base}.{slug_key}.{ext}` where `{base}.{primary_ext}` is the primary
    /// file (e.g. `task_x.json` → `task_x.kv_markdown.md`).
    ///
    /// Returns names only (not full paths). Errors are swallowed and an empty
    /// vec returned — this is a best-effort hint surface, not a load-bearing
    /// lookup.
    pub async fn find_companions(&self, primary_filename: &str) -> Vec<String> {
        // Strip the last extension to get the base prefix shared with companions.
        let Some((base, _)) = primary_filename.rsplit_once('.') else {
            return Vec::new();
        };
        let prefix = format!("{base}.");

        let Ok(files) = self.list_files().await else {
            return Vec::new();
        };
        files
            .into_iter()
            .filter(|name| name.starts_with(&prefix) && name != primary_filename)
            .collect()
    }

    /// Clean up the scratchpad directory and all its contents.
    pub async fn cleanup(&self) {
        match fs::remove_dir_all(&self.dir).await {
            Ok(()) => info!("Scratchpad cleaned up: {}", self.dir.display()),
            Err(e) => warn!(
                "Failed to clean up scratchpad {}: {}",
                self.dir.display(),
                e
            ),
        }
    }
}

/// Replace path-unsafe characters in a single filename component. The key
/// comes from untrusted JSON; we keep alphanumerics, `-`, `_`; everything
/// else (including `/`, `\`, `.`) becomes `_`. An empty result falls back
/// to `"_"` so the filename is never degenerate.
fn slugify_component(key: &str) -> String {
    let slug: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "_".to_string()
    } else {
        slug
    }
}

/// Normalize a path by resolving `.` and `..` components lexically.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

#[derive(Debug, thiserror::Error)]
pub enum ScratchpadPathError {
    #[error("Path '{path}' is outside scratchpad directory '{dir}'")]
    OutsideDirectory { path: String, dir: String },
    #[error("File not found: {0}")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_write_and_read() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-1")
            .await
            .unwrap();

        let content = r#"{"key": "value"}"#;
        let result = storage.write_output("call-1", content).await.unwrap();
        assert_eq!(result.format, ContentFormat::Json);
        assert!(result.path.to_string_lossy().ends_with("call-1.json"));

        // JSON is pretty-printed on disk
        let read_back = fs::read_to_string(&result.path).await.unwrap();
        assert_eq!(read_back, "{\n  \"key\": \"value\"\n}");
        assert_eq!(result.line_count, 3);
        assert!(result.companions.is_empty());
    }

    #[tokio::test]
    async fn test_text_format_detection() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-2")
            .await
            .unwrap();

        let result = storage
            .write_output("call-1", "plain text output")
            .await
            .unwrap();
        assert_eq!(result.format, ContentFormat::Text);
    }

    #[tokio::test]
    async fn test_markdown_detection() {
        // Header + list: positive (legacy case).
        assert!(ContentFormat::is_markdown(
            "### Header\n- key: value\n- key2: value2"
        ));
        // Plain text with no markers: negative.
        assert!(!ContentFormat::is_markdown("just plain text"));
        // Header alone is now sufficient — Dom flagged the prior list
        // co-requirement as too restrictive.
        assert!(ContentFormat::is_markdown("### Header only, no lists"));
        // List alone is still not enough — bullets appear in lots of
        // non-markdown text and would inflate false positives.
        assert!(!ContentFormat::is_markdown("- list only, no headers"));

        // All six ATX header levels match.
        for level in 1..=6 {
            let line = format!("{} Title at level {level}", "#".repeat(level));
            assert!(
                ContentFormat::is_markdown(&line),
                "level {level} header should detect: {line:?}"
            );
        }
        // Seven hashes is not a valid ATX header.
        assert!(!ContentFormat::is_markdown("####### Too many hashes"));

        // The space-after-hash rule rejects the leading-`#` patterns that
        // motivated the original list co-requirement.
        assert!(!ContentFormat::is_markdown("#!/bin/bash\necho hi"));
        assert!(!ContentFormat::is_markdown("#include <stdio.h>"));
        assert!(!ContentFormat::is_markdown("#TODO write the function"));

        // Tab after `#` is also a valid ATX separator per CommonMark §4.2.
        assert!(ContentFormat::is_markdown("#\tTabbed header"));

        // CommonMark allows up to 3 leading spaces before the hashes;
        // we're slightly more permissive (any leading whitespace).
        assert!(ContentFormat::is_markdown("   ## Indented header"));

        // A header anywhere in the first 50 lines is enough — earlier
        // non-header lines should not poison the detection.
        let mut lines = vec!["plain prose".to_string(); 30];
        lines.push("## Suddenly a header".to_string());
        assert!(ContentFormat::is_markdown(&lines.join("\n")));
    }

    #[tokio::test]
    async fn test_companion_extraction() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-comp")
            .await
            .unwrap();

        // JSON with a large markdown string value
        let md_content = (0..15)
            .map(|i| format!("### Section {i}\n- key: value"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let json = serde_json::json!({
            "status": "ok",
            "kv_markdown": md_content,
        });
        let result = storage
            .write_output("call-comp", &json.to_string())
            .await
            .unwrap();

        assert_eq!(result.format, ContentFormat::Json);
        assert_eq!(result.companions.len(), 1);

        let companion = &result.companions[0];
        assert_eq!(companion.source_key, "kv_markdown");
        assert_eq!(companion.format, ContentFormat::Markdown);
        assert!(companion.filename.ends_with(".md"));

        // Verify companion file was written with raw string content
        let companion_path = storage.dir().join(&companion.filename);
        let read_back = fs::read_to_string(&companion_path).await.unwrap();
        assert!(read_back.contains("### Section 0"));
        assert!(read_back.contains("- key: value"));
    }

    #[tokio::test]
    async fn test_companion_path_traversal_is_sanitized() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-traversal")
            .await
            .unwrap();

        // A malicious tool output with a JSON key that tries to escape the
        // scratchpad directory. Content is large enough to trigger companion
        // extraction (markdown with headers + bullets).
        let md = (0..15)
            .map(|i| format!("### Section {i}\n- key: value {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let json = serde_json::json!({
            "status": "ok",
            "../../etc/evil": md,
        });

        let result = storage
            .write_output("call-traverse", &json.to_string())
            .await
            .unwrap();

        assert_eq!(result.companions.len(), 1);
        let companion = &result.companions[0];

        // source_key preserves the raw key (for diagnostics); filename is slugified.
        assert_eq!(companion.source_key, "../../etc/evil");
        assert!(
            !companion.filename.contains(".."),
            "filename should not contain traversal: {}",
            companion.filename
        );
        assert!(
            !companion.filename.contains('/'),
            "filename should not contain '/': {}",
            companion.filename
        );

        // Path must resolve inside the scratchpad dir.
        let written = storage.dir().join(&companion.filename);
        assert!(
            written.starts_with(storage.dir()),
            "companion escaped scratchpad dir: {}",
            written.display()
        );
        assert!(
            tokio::fs::try_exists(&written).await.unwrap(),
            "companion file should exist at the sanitized path"
        );
    }

    #[tokio::test]
    async fn test_companion_not_extracted_for_small_strings() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-small")
            .await
            .unwrap();

        let json = serde_json::json!({"status": "ok", "note": "### Title\n- short"});
        let result = storage
            .write_output("call-small", &json.to_string())
            .await
            .unwrap();
        assert!(result.companions.is_empty());
    }

    #[tokio::test]
    async fn test_companion_extraction_escaped_json() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-esc")
            .await
            .unwrap();

        // Build an inner JSON object with enough keys to exceed COMPANION_MIN_LINES
        // when pretty-printed
        let inner: serde_json::Value = serde_json::json!({
            "data": (0..10).map(|i| serde_json::json!({
                "id": i,
                "name": format!("item_{}", i),
                "value": i * 100,
            })).collect::<Vec<_>>(),
            "total": 10,
        });
        // Serialize the inner JSON to a minified string (this is the "escaped JSON" value)
        let inner_str = serde_json::to_string(&inner).unwrap();

        // Wrap it as a string value in the outer JSON
        let outer = serde_json::json!({
            "status": "ok",
            "payload": inner_str,
        });

        let result = storage
            .write_output("call-esc", &outer.to_string())
            .await
            .unwrap();

        assert_eq!(result.companions.len(), 1);
        let companion = &result.companions[0];
        assert_eq!(companion.source_key, "payload");
        assert_eq!(companion.format, ContentFormat::Json);
        assert!(companion.filename.ends_with(".json"));

        // Verify companion was pretty-printed
        let companion_path = storage.dir().join(&companion.filename);
        let read_back = fs::read_to_string(&companion_path).await.unwrap();
        assert!(read_back.contains("\"id\": 0"));
        assert!(read_back.contains('\n')); // multi-line (pretty-printed)
    }

    #[tokio::test]
    async fn test_validate_path_ok() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-3")
            .await
            .unwrap();

        let result = storage.validate_path("call-1.json");
        assert!(result.is_ok());
    }

    /// With a widened `read_root`, a file that lives outside the scratchpad
    /// write dir but under the read root (e.g. a result artifact) is readable
    /// via a relative `../` path — without copying it into the scratchpad.
    #[tokio::test]
    async fn test_read_root_allows_sibling_artifact() {
        let tmp = TempDir::new().unwrap();
        // Layout: {run}/iteration-1/scratchpad  (write dir)
        //         {run}/artifacts/result.txt    (sibling, under read_root={run})
        let run_dir = tmp.path().join("run-1");
        let scratch_parent = run_dir.join("iteration-1");
        let storage = ScratchpadStorage::in_dir(&scratch_parent)
            .await
            .unwrap()
            .with_read_root(run_dir.clone());

        let artifacts = run_dir.join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        let artifact = artifacts.join("result.txt");
        std::fs::write(&artifact, "artifact content").unwrap();

        // relative_ref builds the token the read tools accept.
        let token = storage.relative_ref(&artifact).unwrap();
        assert!(token.contains("artifacts/result.txt"), "got token: {token}");

        // The token validates and resolves to the artifact on disk.
        let resolved = storage.validate_path(&token).unwrap();
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(&artifact).unwrap()
        );
    }

    /// A path that escapes the widened `read_root` is still rejected.
    #[tokio::test]
    async fn test_read_root_escape_rejected() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-1");
        let scratch_parent = run_dir.join("iteration-1");
        let storage = ScratchpadStorage::in_dir(&scratch_parent)
            .await
            .unwrap()
            .with_read_root(run_dir.clone());

        // ../../.. climbs above the run dir (read_root) → rejected.
        let result = storage.validate_path("../../../etc/passwd");
        assert!(result.is_err(), "path escaping read_root must be rejected");
    }

    /// `relative_ref` rejects an absolute path that is not under the read root.
    #[tokio::test]
    async fn test_relative_ref_outside_read_root_rejected() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-1");
        let scratch_parent = run_dir.join("iteration-1");
        let storage = ScratchpadStorage::in_dir(&scratch_parent)
            .await
            .unwrap()
            .with_read_root(run_dir.clone());

        let outside = tmp.path().join("other").join("secret.txt");
        assert!(storage.relative_ref(&outside).is_err());
    }

    /// `with_read_root` ignores a root that is not an ancestor of the write
    /// dir, preserving the invariant that the scratchpad dir stays readable.
    #[tokio::test]
    async fn test_with_read_root_ignores_non_ancestor() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-rr")
            .await
            .unwrap();
        let dir = storage.dir().to_path_buf();
        let unrelated = tmp.path().join("unrelated");
        let storage = storage.with_read_root(unrelated);
        // read_root falls back to the write dir.
        assert_eq!(storage.read_root(), dir.as_path());
    }

    #[tokio::test]
    async fn test_validate_path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-4")
            .await
            .unwrap();

        let result = storage.validate_path("../../etc/passwd");
        assert!(result.is_err());
    }

    /// Defense-in-depth: an attacker-planted symlink inside the scratchpad
    /// dir pointing outside is rejected by the canonicalization stage.
    ///
    /// Threat model: only matters if some other process can write to
    /// `memory_dir` and create symlinks there. Aura itself never creates
    /// symlinks, but on a shared host (multi-tenant `/tmp`) this guard
    /// closes the gap.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_validate_path_symlink_escape_rejected() {
        let tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        // A real file outside the scratchpad dir we'd love to read.
        let secret = outside_tmp.path().join("secret.txt");
        std::fs::write(&secret, "out of bounds").unwrap();

        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-symlink")
            .await
            .unwrap();
        // Plant a symlink inside the scratchpad dir that points at `secret`.
        let symlink_path = storage.dir().join("link.txt");
        std::os::unix::fs::symlink(&secret, &symlink_path).unwrap();

        // Lexical check passes (the path string is "link.txt", inside the
        // dir), but the canonicalization step must catch the escape.
        let result = storage.validate_path("link.txt");
        assert!(
            result.is_err(),
            "validate_path must reject a symlink that escapes the scratchpad dir"
        );
    }

    /// Non-existent paths still pass the lexical check (used during writes
    /// to a new file). Symlink stage is correctly skipped — no `ENOENT`
    /// false-positive.
    #[tokio::test]
    async fn test_validate_path_nonexistent_path_passes() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-nx")
            .await
            .unwrap();
        let result = storage.validate_path("not_yet_written.json");
        assert!(
            result.is_ok(),
            "non-existent paths inside the dir must validate (used during writes)"
        );
    }

    #[tokio::test]
    async fn test_list_files() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-5")
            .await
            .unwrap();

        storage.write_output("a", "content a").await.unwrap();
        storage.write_output("b", r#"{"x":1}"#).await.unwrap();

        let files = storage.list_files().await.unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"a.txt".to_string()));
        assert!(files.contains(&"b.json".to_string()));
    }

    #[tokio::test]
    async fn test_cleanup() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-6")
            .await
            .unwrap();

        storage.write_output("c", "data").await.unwrap();
        assert!(storage.dir().exists());

        storage.cleanup().await;
        assert!(!storage.dir().exists());
    }
}
