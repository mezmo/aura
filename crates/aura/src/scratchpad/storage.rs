//! Scratchpad storage: file I/O, path validation, format detection, cleanup.

use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info, warn};

/// Detected format of scratchpad content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentFormat {
    Json,
    Text,
}

impl ContentFormat {
    /// Detect format from content by checking if it parses as JSON.
    pub fn detect(content: &str) -> Self {
        let trimmed = content.trim();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
        {
            ContentFormat::Json
        } else {
            ContentFormat::Text
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ContentFormat::Json => "json",
            ContentFormat::Text => "text",
        }
    }
}

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

    /// Write tool output to a scratchpad file.
    ///
    /// Returns the path to the written file and the detected format.
    pub async fn write_output(
        &self,
        tool_call_id: &str,
        content: &str,
    ) -> std::io::Result<(PathBuf, ContentFormat)> {
        let format = ContentFormat::detect(content);
        let ext = match format {
            ContentFormat::Json => "json",
            ContentFormat::Text => "txt",
        };
        let filename = format!("{tool_call_id}.{ext}");
        let path = self.dir.join(&filename);
        fs::write(&path, content).await?;
        debug!(
            "Scratchpad file written: {} ({} bytes, {})",
            path.display(),
            content.len(),
            format.as_str()
        );
        Ok((path, format))
    }

    /// Write tool output synchronously (for use in sync callbacks like `transform_output`).
    pub fn write_output_sync(
        &self,
        tool_call_id: &str,
        content: &str,
    ) -> std::io::Result<(PathBuf, ContentFormat)> {
        let format = ContentFormat::detect(content);
        let ext = match format {
            ContentFormat::Json => "json",
            ContentFormat::Text => "txt",
        };
        let filename = format!("{tool_call_id}.{ext}");
        let path = self.dir.join(&filename);
        std::fs::write(&path, content)?;
        debug!(
            "Scratchpad file written: {} ({} bytes, {})",
            path.display(),
            content.len(),
            format.as_str()
        );
        Ok((path, format))
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
        let (path, format) = storage.write_output("call-1", content).await.unwrap();
        assert_eq!(format, ContentFormat::Json);
        assert!(path.to_string_lossy().ends_with("call-1.json"));

        let read_back = fs::read_to_string(&path).await.unwrap();
        assert_eq!(read_back, content);
    }

    #[tokio::test]
    async fn test_text_format_detection() {
        let tmp = TempDir::new().unwrap();
        let storage = ScratchpadStorage::with_base_dir(tmp.path(), "req-2")
            .await
            .unwrap();

        let (_, format) = storage
            .write_output("call-1", "plain text output")
            .await
            .unwrap();
        assert_eq!(format, ContentFormat::Text);
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
