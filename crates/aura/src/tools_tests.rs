#[cfg(test)]
mod tests {
    use crate::tools::*;
    use rig::tool::Tool;
    use std::fs;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_temp_dir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("aura_test_{}_{}", pid, id));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn create_test_file_for_write(dir: &TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, "").unwrap();
        path
    }

    fn create_test_file(dir: &TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_filesystem_tool_new() {
        let tool = FilesystemTool::new();
        assert_eq!(tool.base_dir, None);
        assert!(!tool.allow_write);
        assert_eq!(tool.max_file_size, 1_048_576);
    }

    #[test]
    fn test_filesystem_tool_default() {
        let tool = FilesystemTool::default();
        assert_eq!(tool.base_dir, None);
        assert!(!tool.allow_write);
        assert_eq!(tool.max_file_size, 1_048_576);
    }

    #[test]
    fn test_filesystem_tool_with_base_dir() {
        let tool = FilesystemTool::new().with_base_dir("/tmp".to_string());
        assert_eq!(tool.base_dir, Some("/tmp".to_string()));
    }

    #[test]
    fn test_filesystem_tool_with_write_access() {
        let tool = FilesystemTool::new().with_write_access(true);
        assert!(tool.allow_write);
        let tool = FilesystemTool::new().with_write_access(false);
        assert!(!tool.allow_write);
    }

    #[test]
    fn test_filesystem_tool_with_max_file_size() {
        let tool = FilesystemTool::new().with_max_file_size(2048);
        assert_eq!(tool.max_file_size, 2048);
    }

    #[test]
    fn test_filesystem_tool_builder_chain() {
        let tool = FilesystemTool::new()
            .with_base_dir("/tmp".to_string())
            .with_write_access(true)
            .with_max_file_size(4096);
        assert_eq!(tool.base_dir, Some("/tmp".to_string()));
        assert!(tool.allow_write);
        assert_eq!(tool.max_file_size, 4096);
    }

    #[tokio::test]
    async fn test_read_file_tool_success() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "test.txt", "hello world");
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_read_file_tool_empty_file() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "empty.txt", "");
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    #[tokio::test]
    async fn test_read_file_tool_nonexistent() {
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: "/nonexistent/file.txt".to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FilesystemError::PathNotFound(_)));
    }

    #[tokio::test]
    async fn test_read_file_tool_max_size_exceeded() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(100);
        let file_path = create_test_file(&temp_dir, "large.txt", &content);
        
        let tool = ReadFileTool(FilesystemTool::new().with_max_file_size(50));
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_read_file_tool_max_size_exact() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(100);
        let file_path = create_test_file(&temp_dir, "exact.txt", &content);
        
        let tool = ReadFileTool(FilesystemTool::new().with_max_file_size(100));
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn test_read_file_tool_custom_max_size() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(100);
        let file_path = create_test_file(&temp_dir, "custom.txt", &content);
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: Some(50),
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_read_file_tool_directory_not_file() {
        let temp_dir = create_temp_dir();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FilesystemError::InvalidPath(_)));
    }

    #[tokio::test]
    async fn test_read_file_tool_unicode_content() {
        let temp_dir = create_temp_dir();
        let content = "Hello 世界 🎉";
        let file_path = create_test_file(&temp_dir, "unicode.txt", content);
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);
    }

    #[tokio::test]
    async fn test_list_dir_tool_success() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "file1.txt", "content1");
        create_test_file(&temp_dir, "file2.txt", "content2");
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let listing = result.unwrap();
        assert_eq!(listing.total_count, 2);
        assert_eq!(listing.entries.len(), 2);
    }

    #[tokio::test]
    async fn test_list_dir_tool_empty_directory() {
        let temp_dir = create_temp_dir();
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let listing = result.unwrap();
        assert_eq!(listing.total_count, 0);
        assert_eq!(listing.entries.len(), 0);
    }

    #[tokio::test]
    async fn test_list_dir_tool_nonexistent() {
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: "/nonexistent/directory".to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(matches!(err, FilesystemError::PathNotFound(_)));
    }

    #[tokio::test]
    async fn test_list_dir_tool_file_not_directory() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "file.txt", "content");
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: file_path.to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(matches!(err, FilesystemError::InvalidPath(_)));
    }

    #[tokio::test]
    async fn test_list_dir_tool_file_info() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "test.txt", "content");
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let listing = result.unwrap();
        assert_eq!(listing.entries.len(), 2);
        
        let file_entry = listing.entries.iter().find(|e| e.name == "test.txt").unwrap();
        assert!(file_entry.is_file);
        assert!(!file_entry.is_dir);
        assert!(file_entry.size.is_some());
        
        let dir_entry = listing.entries.iter().find(|e| e.name == "subdir").unwrap();
        assert!(!dir_entry.is_file);
        assert!(dir_entry.is_dir);
        assert!(dir_entry.size.is_none());
    }

    #[tokio::test]
    async fn test_write_file_tool_success() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "new.txt");

        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "test content".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "test content");
    }

    #[tokio::test]
    async fn test_write_file_tool_write_disabled() {
        let temp_dir = create_temp_dir();
        let file_path = temp_dir.path().join("new.txt");
        
        let tool = WriteFileTool(FilesystemTool::new());
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "content".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_write_file_tool_empty_content() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "empty.txt");
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "");
    }

    #[tokio::test]
    async fn test_write_file_tool_unicode_content() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "unicode.txt");
        let content = "Hello 世界 🎉";
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: content.to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let read_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(read_content, content);
    }

    #[tokio::test]
    async fn test_write_file_tool_overwrite() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "overwrite.txt", "original");
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "new content".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "new content");
    }

    #[tokio::test]
    async fn test_write_file_tool_create_dirs() {
        let temp_dir = create_temp_dir();
        let nested_dir = temp_dir.path().join("subdir").join("nested");
        fs::create_dir_all(&nested_dir).unwrap();
        let file_path = nested_dir.join("file.txt");
        fs::write(&file_path, "").unwrap();

        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "content".to_string(),
            create_dirs: Some(true),
        };
        let result = tool.call(args).await;

        assert!(result.is_ok());
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "content");
    }

    #[tokio::test]
    async fn test_read_file_tool_name() {
        assert_eq!(ReadFileTool::NAME, "read_file");
    }

    #[tokio::test]
    async fn test_read_file_tool_definition() {
        let fs_tool = FilesystemTool::new();
        let tool = ReadFileTool(fs_tool);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "read_file");
        assert!(definition.description.contains("Read"));
        assert!(definition.parameters.get("type").is_some());
    }

    #[tokio::test]
    async fn test_read_file_tool_call_with_max_size() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "test.txt", "hello");
        
        let fs_tool = FilesystemTool::new();
        let tool = ReadFileTool(fs_tool);
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: Some(10),
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_list_dir_tool_name() {
        assert_eq!(ListDirTool::NAME, "list_directory");
    }

    #[tokio::test]
    async fn test_list_dir_tool_definition() {
        let fs_tool = FilesystemTool::new();
        let tool = ListDirTool(fs_tool);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "list_directory");
        assert!(definition.description.contains("List"));
        assert!(definition.parameters.get("type").is_some());
    }

    #[tokio::test]
    async fn test_list_dir_tool_call_recursive_false() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "file.txt", "content");
        
        let fs_tool = FilesystemTool::new();
        let tool = ListDirTool(fs_tool);
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: Some(false),
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_list_dir_tool_call_recursive_true() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "file.txt", "content");
        
        let fs_tool = FilesystemTool::new();
        let tool = ListDirTool(fs_tool);
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: Some(true),
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_write_file_tool_name() {
        assert_eq!(WriteFileTool::NAME, "write_file");
    }

    #[tokio::test]
    async fn test_write_file_tool_definition() {
        let fs_tool = FilesystemTool::new();
        let tool = WriteFileTool(fs_tool);
        let definition = tool.definition(String::new()).await;
        
        assert_eq!(definition.name, "write_file");
        assert!(definition.description.contains("Write"));
        assert!(definition.parameters.get("type").is_some());
    }

    #[tokio::test]
    async fn test_write_file_tool_call_create_dirs_false() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "new.txt");
        
        let fs_tool = FilesystemTool::new().with_write_access(true);
        let tool = WriteFileTool(fs_tool);
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "test".to_string(),
            create_dirs: Some(false),
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_write_file_tool_call_create_dirs_true() {
        let temp_dir = create_temp_dir();
        let subdir = temp_dir.path().join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        let file_path = subdir.join("new.txt");
        fs::write(&file_path, "").unwrap();
        
        let fs_tool = FilesystemTool::new().with_write_access(true);
        let tool = WriteFileTool(fs_tool);
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "test".to_string(),
            create_dirs: Some(true),
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[test]
    fn test_filesystem_error_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let fs_err = FilesystemError::from(io_err);
        assert!(matches!(fs_err, FilesystemError::IoError(_)));
    }

    #[test]
    fn test_filesystem_error_permission_denied() {
        let err = FilesystemError::PermissionDenied("test".to_string());
        assert!(err.to_string().contains("Permission denied"));
    }

    #[test]
    fn test_filesystem_error_path_not_found() {
        let err = FilesystemError::PathNotFound("test".to_string());
        assert!(err.to_string().contains("Path not found"));
    }

    #[test]
    fn test_filesystem_error_invalid_path() {
        let err = FilesystemError::InvalidPath("test".to_string());
        assert!(err.to_string().contains("Invalid path"));
    }

    #[test]
    fn test_read_file_args_serde() {
        let args = ReadFileArgs {
            path: "/test/path".to_string(),
            max_size: Some(1024),
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: ReadFileArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, "/test/path");
        assert_eq!(deserialized.max_size, Some(1024));
    }

    #[test]
    fn test_read_file_args_serde_none() {
        let args = ReadFileArgs {
            path: "/test/path".to_string(),
            max_size: None,
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: ReadFileArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, "/test/path");
        assert_eq!(deserialized.max_size, None);
    }

    #[test]
    fn test_list_dir_args_serde() {
        let args = ListDirArgs {
            path: "/test/path".to_string(),
            recursive: Some(true),
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: ListDirArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, "/test/path");
        assert_eq!(deserialized.recursive, Some(true));
    }

    #[test]
    fn test_write_file_args_serde() {
        let args = WriteFileArgs {
            path: "/test/path".to_string(),
            content: "test content".to_string(),
            create_dirs: Some(true),
        };
        let json = serde_json::to_string(&args).unwrap();
        let deserialized: WriteFileArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, "/test/path");
        assert_eq!(deserialized.content, "test content");
        assert_eq!(deserialized.create_dirs, Some(true));
    }

    #[test]
    fn test_filesystem_tool_clone() {
        let tool = FilesystemTool::new()
            .with_base_dir("/tmp".to_string())
            .with_write_access(true)
            .with_max_file_size(2048);
        let cloned = tool.clone();
        assert_eq!(cloned.base_dir, tool.base_dir);
        assert_eq!(cloned.allow_write, tool.allow_write);
        assert_eq!(cloned.max_file_size, tool.max_file_size);
    }

    #[test]
    fn test_read_file_tool_clone() {
        let fs_tool = FilesystemTool::new();
        let tool = ReadFileTool(fs_tool);
        let _cloned = tool.clone();
    }

    #[test]
    fn test_list_dir_tool_clone() {
        let fs_tool = FilesystemTool::new();
        let tool = ListDirTool(fs_tool);
        let _cloned = tool.clone();
    }

    #[test]
    fn test_write_file_tool_clone() {
        let fs_tool = FilesystemTool::new();
        let tool = WriteFileTool(fs_tool);
        let _cloned = tool.clone();
    }

    #[tokio::test]
    async fn test_read_file_tool_large_file() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(2_000_000);
        let file_path = create_test_file(&temp_dir, "large.txt", &content);
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_file_tool_zero_max_size() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "test.txt", "content");
        
        let tool = ReadFileTool(FilesystemTool::new().with_max_file_size(0));
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_dir_tool_with_subdirectories() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "file.txt", "content");
        fs::create_dir(temp_dir.path().join("subdir1")).unwrap();
        fs::create_dir(temp_dir.path().join("subdir2")).unwrap();
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let listing = result.unwrap();
        assert_eq!(listing.total_count, 3);
    }

    #[tokio::test]
    async fn test_write_file_tool_large_content() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "large.txt");
        let content = "a".repeat(10_000);
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: content.clone(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let read_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(read_content.len(), 10_000);
    }

    #[tokio::test]
    async fn test_write_file_tool_newlines() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "newlines.txt");
        let content = "line1\nline2\nline3";
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: content.to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let read_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(read_content, content);
    }

    #[tokio::test]
    async fn test_read_file_tool_with_max_size_zero() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "test.txt", "content");
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: Some(0),
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_dir_tool_unicode_filenames() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "文件.txt", "content");
        create_test_file(&temp_dir, "файл.txt", "content");
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let listing = result.unwrap();
        assert_eq!(listing.total_count, 2);
    }

    #[tokio::test]
    async fn test_write_file_tool_special_characters() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "special.txt");
        let content = "!@#$%^&*()_+-=[]{}|;':\",./<>?";
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: content.to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
        let read_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(read_content, content);
    }

    #[tokio::test]
    async fn test_read_file_tool_max_file_size_boundary() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(1_048_576);
        let file_path = create_test_file(&temp_dir, "boundary.txt", &content);
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_read_file_tool_max_file_size_one_over() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(1_048_577);
        let file_path = create_test_file(&temp_dir, "over.txt", &content);
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_file_tool_with_base_dir_allowed() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file(&temp_dir, "test.txt", "content");
        
        let tool = ReadFileTool(
            FilesystemTool::new()
                .with_base_dir(temp_dir.path().to_str().unwrap().to_string())
        );
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_read_file_tool_with_base_dir_denied() {
        let temp_dir1 = create_temp_dir();
        let temp_dir2 = create_temp_dir();
        let file_path = create_test_file(&temp_dir2, "test.txt", "content");
        
        let tool = ReadFileTool(
            FilesystemTool::new()
                .with_base_dir(temp_dir1.path().to_str().unwrap().to_string())
        );
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_list_dir_tool_with_base_dir_allowed() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "test.txt", "content");
        
        let tool = ListDirTool(
            FilesystemTool::new()
                .with_base_dir(temp_dir.path().to_str().unwrap().to_string())
        );
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_list_dir_tool_with_base_dir_denied() {
        let temp_dir1 = create_temp_dir();
        let temp_dir2 = create_temp_dir();
        
        let tool = ListDirTool(
            FilesystemTool::new()
                .with_base_dir(temp_dir1.path().to_str().unwrap().to_string())
        );
        let args = ListDirArgs {
            path: temp_dir2.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_write_file_tool_with_base_dir_allowed() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "new.txt");
        
        let tool = WriteFileTool(
            FilesystemTool::new()
                .with_base_dir(temp_dir.path().to_str().unwrap().to_string())
                .with_write_access(true)
        );
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "test".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_write_file_tool_with_base_dir_denied() {
        let temp_dir1 = create_temp_dir();
        let temp_dir2 = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir2, "new.txt");
        
        let tool = WriteFileTool(
            FilesystemTool::new()
                .with_base_dir(temp_dir1.path().to_str().unwrap().to_string())
                .with_write_access(true)
        );
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "test".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FilesystemError::PermissionDenied(_)));
    }

    #[test]
    fn test_read_file_args_empty_path() {
        let args = ReadFileArgs {
            path: "".to_string(),
            max_size: None,
        };
        assert_eq!(args.path, "");
    }

    #[test]
    fn test_list_dir_args_empty_path() {
        let args = ListDirArgs {
            path: "".to_string(),
            recursive: None,
        };
        assert_eq!(args.path, "");
    }

    #[test]
    fn test_write_file_args_empty_path() {
        let args = WriteFileArgs {
            path: "".to_string(),
            content: "test".to_string(),
            create_dirs: None,
        };
        assert_eq!(args.path, "");
    }

    #[test]
    fn test_write_file_args_empty_content() {
        let args = WriteFileArgs {
            path: "/test/path".to_string(),
            content: "".to_string(),
            create_dirs: None,
        };
        assert_eq!(args.content, "");
    }
}
