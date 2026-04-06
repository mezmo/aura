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
    /// Detect format from content by checking structure markers.
    pub fn detect(content: &str) -> Self {
        Self::detect_and_parse(content).0
    }

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
    /// Looks for markdown headers (`#`, `##`, `###`) with structured list content.
    pub fn is_markdown(content: &str) -> bool {
        let mut has_header = false;
        let mut has_list = false;
        for line in content.lines().take(50) {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                has_header = true;
            }
            if trimmed.starts_with("- ") {
                has_list = true;
            }
            if has_header && has_list {
                return true;
            }
        }
        false
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
    /// Directory for this request's scratchpad files.
    dir: PathBuf,
}

impl ScratchpadStorage {
    /// Create scratchpad storage under the given directory.
    ///
    /// Creates `{parent}/scratchpad/` for storing intercepted tool outputs.
    pub async fn in_dir(parent: &Path) -> std::io::Result<Self> {
        let dir = parent.join("scratchpad");
        fs::create_dir_all(&dir).await?;
        info!("Scratchpad directory created: {}", dir.display());
        Ok(Self { dir })
    }

    /// Create storage with a specific base directory (for testing).
    pub async fn with_base_dir(base: &Path, request_id: &str) -> std::io::Result<Self> {
        let dir = base.join(request_id);
        fs::create_dir_all(&dir).await?;
        Ok(Self { dir })
    }

    /// Get the scratchpad directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
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
            let path = self.dir.join(&pending.filename);
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
            let path = self.dir.join(&pending.filename);
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

    /// Determine which companion files to extract from a JSON value.
    ///
    /// Checks top-level string values for structured content:
    /// 1. Escaped JSON strings → extracted as pretty-printed `.json`
    /// 2. Markdown (headers + lists) → extracted as `.md`
    /// 3. Large plain text (≥ COMPANION_MIN_LINES) → extracted as `.txt`
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
        let filename = format!("{tool_call_id}.{key}.{}", format.extension());
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
    /// Returns the canonicalized path if valid, or an error.
    pub fn validate_path(&self, file_path: &str) -> Result<PathBuf, ScratchpadPathError> {
        let requested = self.dir.join(file_path);

        // Use lexical normalization since the file may not exist yet for canonicalize
        let normalized = normalize_path(&requested);

        if !normalized.starts_with(&self.dir) {
            return Err(ScratchpadPathError::OutsideDirectory {
                path: file_path.to_string(),
                dir: self.dir.display().to_string(),
            });
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
        assert!(ContentFormat::is_markdown(
            "### Header\n- key: value\n- key2: value2"
        ));
        assert!(!ContentFormat::is_markdown("just plain text"));
        assert!(!ContentFormat::is_markdown("### Header only, no lists"));
        assert!(!ContentFormat::is_markdown("- list only, no headers"));
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

    #[tokio::test]
    async fn test_validate_path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-4")
            .await
            .unwrap();

        let result = storage.validate_path("../../etc/passwd");
        assert!(result.is_err());
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
