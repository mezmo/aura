use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use tokio::fs;
use tracing::{debug, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum FilesystemError {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Path not found: {0}")]
    PathNotFound(String),

    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

#[derive(Deserialize, Serialize)]
pub struct ReadFileArgs {
    pub path: String,
    pub max_size: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub struct ListDirArgs {
    pub path: String,
    pub recursive: Option<bool>,
}

#[derive(Deserialize, Serialize)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
    pub create_dirs: Option<bool>,
}

#[derive(Serialize)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub is_file: bool,
    pub is_dir: bool,
    pub size: Option<u64>,
}

#[derive(Serialize)]
pub struct DirectoryListing {
    pub path: String,
    pub entries: Vec<FileInfo>,
    pub total_count: usize,
}

/// Filesystem tool for reading and writing files
/// Implements security restrictions to prevent access to sensitive areas
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemTool {
    /// Base directory to restrict access to (optional)
    pub base_dir: Option<String>,
    /// Whether to allow writing files
    pub allow_write: bool,
    /// Maximum file size to read (in bytes)
    pub max_file_size: usize,
}

impl Default for FilesystemTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesystemTool {
    pub fn new() -> Self {
        Self {
            base_dir: None,
            allow_write: false,
            max_file_size: 1_048_576, // 1MB default
        }
    }

    pub fn with_base_dir(mut self, base_dir: String) -> Self {
        self.base_dir = Some(base_dir);
        self
    }

    pub fn with_write_access(mut self, allow: bool) -> Self {
        self.allow_write = allow;
        self
    }

    pub fn with_max_file_size(mut self, size: usize) -> Self {
        self.max_file_size = size;
        self
    }

    /// Validate and normalize a path for security
    fn validate_path(&self, path: &str) -> Result<std::path::PathBuf, FilesystemError> {
        let path = Path::new(path);

        // Convert to absolute path
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(FilesystemError::IoError)?
                .join(path)
        };

        // Normalize the path (resolve .. and .)
        let normalized = abs_path
            .canonicalize()
            .map_err(|_| FilesystemError::PathNotFound(path.display().to_string()))?;

        // If base_dir is set, ensure path is within it
        if let Some(ref base_dir) = self.base_dir {
            let base_path = Path::new(base_dir).canonicalize().map_err(|_| {
                FilesystemError::InvalidPath(format!("Invalid base directory: {base_dir}"))
            })?;

            if !normalized.starts_with(&base_path) {
                return Err(FilesystemError::PermissionDenied(format!(
                    "Access denied: path outside of base directory {base_dir}"
                )));
            }
        }

        // Block access to sensitive directories
        let path_str = normalized.to_string_lossy().to_lowercase();
        let sensitive_patterns = [
            "/etc", "/proc", "/sys", "/dev", "/var/log", "/.ssh", "/.aws", "/.config",
        ];

        for pattern in &sensitive_patterns {
            if path_str.contains(pattern) {
                return Err(FilesystemError::PermissionDenied(format!(
                    "Access denied to sensitive directory: {pattern}"
                )));
            }
        }

        Ok(normalized)
    }

    /// Read file contents
    async fn read_file_impl(
        &self,
        path: &str,
        max_size: Option<usize>,
    ) -> Result<String, FilesystemError> {
        let validated_path = self.validate_path(path)?;
        debug!("Reading file: {}", validated_path.display());
        info!("📄 Reading file: {}", validated_path.display());

        // Check file size
        let metadata = fs::metadata(&validated_path).await?;
        if !metadata.is_file() {
            return Err(FilesystemError::InvalidPath(
                "Path is not a file".to_string(),
            ));
        }

        let file_size = metadata.len() as usize;
        let size_limit = max_size.unwrap_or(self.max_file_size);

        if file_size > size_limit {
            return Err(FilesystemError::PermissionDenied(format!(
                "File too large: {file_size} bytes (limit: {size_limit} bytes)"
            )));
        }

        // Read the file
        let content = fs::read_to_string(&validated_path).await?;
        info!(
            "✅ File read completed: {} ({} bytes)",
            validated_path.display(),
            content.len()
        );
        Ok(content)
    }

    /// List directory contents
    async fn list_dir_impl(
        &self,
        path: &str,
        recursive: bool,
    ) -> Result<DirectoryListing, FilesystemError> {
        let validated_path = self.validate_path(path)?;
        debug!("Listing directory: {}", validated_path.display());
        info!("📁 Listing directory: {}", validated_path.display());

        if !validated_path.is_dir() {
            return Err(FilesystemError::InvalidPath(
                "Path is not a directory".to_string(),
            ));
        }

        let mut entries = Vec::new();
        let mut read_dir = fs::read_dir(&validated_path).await?;

        while let Some(entry) = read_dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            let path = entry.path();

            let file_info = FileInfo {
                name: entry.file_name().to_string_lossy().to_string(),
                path: path.display().to_string(),
                is_file: metadata.is_file(),
                is_dir: metadata.is_dir(),
                size: if metadata.is_file() {
                    Some(metadata.len())
                } else {
                    None
                },
            };

            entries.push(file_info);

            // TODO: Implement recursive listing if requested
            if recursive && metadata.is_dir() {
                warn!("Recursive directory listing not yet implemented");
            }
        }

        let total_count = entries.len();
        info!(
            "✅ Directory listing completed: {} ({} entries)",
            validated_path.display(),
            total_count
        );
        Ok(DirectoryListing {
            path: validated_path.display().to_string(),
            entries,
            total_count,
        })
    }

    /// Write file contents (if allowed)
    async fn write_file_impl(
        &self,
        path: &str,
        content: &str,
        create_dirs: bool,
    ) -> Result<String, FilesystemError> {
        if !self.allow_write {
            return Err(FilesystemError::PermissionDenied(
                "Write access disabled".to_string(),
            ));
        }

        let validated_path = self.validate_path(path)?;
        debug!("Writing file: {}", validated_path.display());

        // Create parent directories if requested
        if create_dirs {
            if let Some(parent) = validated_path.parent() {
                fs::create_dir_all(parent).await?;
            }
        }

        // Write the file
        fs::write(&validated_path, content).await?;

        Ok(format!(
            "File written successfully: {}",
            validated_path.display()
        ))
    }
}

// Read File Tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileTool(pub FilesystemTool);

impl Tool for ReadFileTool {
    const NAME: &'static str = "read_file";
    type Error = FilesystemError;
    type Args = ReadFileArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read the contents of a file from the filesystem".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.0.read_file_impl(&args.path, args.max_size).await
    }
}

// List Directory Tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDirTool(pub FilesystemTool);

impl Tool for ListDirTool {
    const NAME: &'static str = "list_directory";
    type Error = FilesystemError;
    type Args = ListDirArgs;
    type Output = DirectoryListing;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "List the contents of a directory".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the directory to list"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.0
            .list_dir_impl(&args.path, args.recursive.unwrap_or(false))
            .await
    }
}

// Write File Tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileTool(pub FilesystemTool);

impl Tool for WriteFileTool {
    const NAME: &'static str = "write_file";
    type Error = FilesystemError;
    type Args = WriteFileArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Write content to a file on the filesystem".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.0
            .write_file_impl(&args.path, &args.content, args.create_dirs.unwrap_or(false))
            .await
    }
}
