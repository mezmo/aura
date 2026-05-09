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
        
        let content = result.unwrap();
        assert_eq!(content, "hello world");
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
        
        let content = result.unwrap();
        assert_eq!(content, "");
    }

    #[tokio::test]
    async fn test_read_file_tool_nonexistent() {
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: "/nonexistent/file.txt".to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PathNotFound(_)));
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("File too large"));
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
        
        let content = result.unwrap();
        assert_eq!(content.len(), 100);
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::InvalidPath(_)));
        assert!(err.to_string().contains("not a file"));
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
        
        let read_content = result.unwrap();
        assert_eq!(read_content, content);
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
        if let Err(err) = result {
            assert!(matches!(err, FilesystemError::PathNotFound(_)));
        }
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
        if let Err(err) = result {
            assert!(matches!(err, FilesystemError::InvalidPath(_)));
            assert!(err.to_string().contains("not a directory"));
        }
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
        
        let message = result.unwrap();
        assert!(message.contains("File written successfully"));
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("Write access disabled"));
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
        
        result.unwrap();
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
        
        result.unwrap();
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
        
        result.unwrap();
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

        result.unwrap();
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
        assert!(definition.parameters.get("properties").is_some());
        assert!(definition.parameters.get("required").is_some());
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
        
        let content = result.unwrap();
        assert_eq!(content, "hello");
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
        
        let listing = result.unwrap();
        assert_eq!(listing.entries.len(), 1);
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
        
        result.unwrap();
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
        
        result.unwrap();
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
        
        result.unwrap();
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
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
        
        result.unwrap();
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
        
        result.unwrap();
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
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
        
        result.unwrap();
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
        
        let read_content = result.unwrap();
        assert_eq!(read_content.len(), 1_048_576);
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
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
        
        let content = result.unwrap();
        assert_eq!(content, "content");
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("outside of base directory"));
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
        
        let listing = result.unwrap();
        assert_eq!(listing.entries.len(), 1);
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
        if let Err(err) = result {
            assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        }
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
        
        result.unwrap();
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
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
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

    #[tokio::test]
    async fn test_validate_path_blocks_etc() {
        let temp_dir = create_temp_dir();
        let etc_dir = temp_dir.path().join("etc");
        fs::create_dir(&etc_dir).unwrap();
        let file_path = etc_dir.join("passwd");
        fs::write(&file_path, "test").unwrap();

        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;

        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("/etc"));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_proc() {
        let temp_dir = create_temp_dir();
        let proc_dir = temp_dir.path().join("proc");
        fs::create_dir(&proc_dir).unwrap();
        let file_path = proc_dir.join("cpuinfo");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("/proc"));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_sys() {
        let temp_dir = create_temp_dir();
        let sys_dir = temp_dir.path().join("sys");
        fs::create_dir(&sys_dir).unwrap();
        let file_path = sys_dir.join("kernel");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("/sys"));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_dev() {
        let temp_dir = create_temp_dir();
        let dev_dir = temp_dir.path().join("dev");
        fs::create_dir(&dev_dir).unwrap();
        let file_path = dev_dir.join("null");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("/dev"));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_var_log() {
        let temp_dir = create_temp_dir();
        let var_dir = temp_dir.path().join("var");
        fs::create_dir(&var_dir).unwrap();
        let log_dir = var_dir.join("log");
        fs::create_dir(&log_dir).unwrap();
        let file_path = log_dir.join("syslog");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_dot_ssh() {
        let temp_dir = create_temp_dir();
        let ssh_dir = temp_dir.path().join(".ssh");
        fs::create_dir(&ssh_dir).unwrap();
        let file_path = ssh_dir.join("id_rsa");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("/.ssh"));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_dot_aws() {
        let temp_dir = create_temp_dir();
        let aws_dir = temp_dir.path().join(".aws");
        fs::create_dir(&aws_dir).unwrap();
        let file_path = aws_dir.join("credentials");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("/.aws"));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_dot_config() {
        let temp_dir = create_temp_dir();
        let config_dir = temp_dir.path().join(".config");
        fs::create_dir(&config_dir).unwrap();
        let file_path = config_dir.join("config");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        assert!(err.to_string().contains("/.config"));
    }

    #[tokio::test]
    async fn test_validate_path_blocks_etc_in_list_dir() {
        let temp_dir = create_temp_dir();
        let etc_dir = temp_dir.path().join("etc");
        fs::create_dir(&etc_dir).unwrap();
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: etc_dir.to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        assert!(result.is_err());
        if let Err(err) = result {
            assert!(matches!(err, FilesystemError::PermissionDenied(_)));
        }
    }

    #[tokio::test]
    async fn test_validate_path_blocks_ssh_in_write_file() {
        let temp_dir = create_temp_dir();
        let ssh_dir = temp_dir.path().join(".ssh");
        fs::create_dir(&ssh_dir).unwrap();
        let file_path = ssh_dir.join("authorized_keys");
        fs::write(&file_path, "test").unwrap();
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "malicious key".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_validate_path_case_insensitive_etc() {
        let temp_dir = create_temp_dir();
        let etc_dir = temp_dir.path().join("ETC");
        fs::create_dir(&etc_dir).unwrap();
        let file_path = etc_dir.join("passwd");
        fs::write(&file_path, "test").unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_validate_path_relative_path_conversion() {
        let temp_dir = create_temp_dir();
        let _file_path = create_test_file(&temp_dir, "test.txt", "content");

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: "test.txt".to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        std::env::set_current_dir(original_dir).unwrap();
        
        let content = result.unwrap();
        assert_eq!(content, "content");
    }

    #[tokio::test]
    async fn test_file_info_serialization() {
        let file_info = FileInfo {
            name: "test.txt".to_string(),
            path: "/tmp/test.txt".to_string(),
            is_file: true,
            is_dir: false,
            size: Some(1024),
        };
        
        let json = serde_json::to_string(&file_info).unwrap();
        assert!(json.contains("test.txt"));
        assert!(json.contains("1024"));
    }

    #[tokio::test]
    async fn test_directory_listing_serialization() {
        let listing = DirectoryListing {
            path: "/tmp".to_string(),
            entries: vec![
                FileInfo {
                    name: "file1.txt".to_string(),
                    path: "/tmp/file1.txt".to_string(),
                    is_file: true,
                    is_dir: false,
                    size: Some(100),
                },
            ],
            total_count: 1,
        };
        
        let json = serde_json::to_string(&listing).unwrap();
        assert!(json.contains("file1.txt"));
        assert!(json.contains("total_count"));
    }

    #[tokio::test]
    async fn test_list_dir_tool_path_in_result() {
        let temp_dir = create_temp_dir();
        let _file_path = create_test_file(&temp_dir, "test.txt", "content");
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        let listing = result.unwrap();
        assert!(listing.path.contains(temp_dir.path().to_str().unwrap()));
    }

    #[tokio::test]
    async fn test_list_dir_tool_entry_paths() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "test.txt", "content");
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        let listing = result.unwrap();
        let entry = &listing.entries[0];
        assert_eq!(entry.name, "test.txt");
        assert!(entry.path.ends_with("test.txt"));
    }

    #[tokio::test]
    async fn test_write_file_tool_result_message() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "new.txt");
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: "test".to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        let message = result.unwrap();
        assert!(message.contains("File written successfully"));
        assert!(message.contains("new.txt"));
    }

    #[tokio::test]
    async fn test_read_file_tool_multiline_content() {
        let temp_dir = create_temp_dir();
        let content = "line1\nline2\nline3\nline4";
        let file_path = create_test_file(&temp_dir, "multiline.txt", content);
        
        let tool = ReadFileTool(FilesystemTool::new());
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: None,
        };
        let result = tool.call(args).await;
        
        let read_content = result.unwrap();
        assert_eq!(read_content, content);
        assert_eq!(read_content.lines().count(), 4);
    }

    #[tokio::test]
    async fn test_list_dir_tool_file_size_accuracy() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(1234);
        create_test_file(&temp_dir, "sized.txt", &content);
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        let listing = result.unwrap();
        let entry = listing.entries.iter().find(|e| e.name == "sized.txt").unwrap();
        assert_eq!(entry.size, Some(1234));
    }

    #[tokio::test]
    async fn test_write_file_tool_preserves_exact_content() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "exact.txt");
        let content = "exact content with spaces   and\ttabs\nand newlines";
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: content.to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        result.unwrap();
        let read_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(read_content, content);
    }

    #[tokio::test]
    async fn test_filesystem_tool_max_file_size_custom_values() {
        let tool1 = FilesystemTool::new().with_max_file_size(1);
        assert_eq!(tool1.max_file_size, 1);
        
        let tool2 = FilesystemTool::new().with_max_file_size(999_999_999);
        assert_eq!(tool2.max_file_size, 999_999_999);
    }

    #[tokio::test]
    async fn test_read_file_tool_respects_custom_max_size_over_default() {
        let temp_dir = create_temp_dir();
        let content = "a".repeat(100);
        let file_path = create_test_file(&temp_dir, "test.txt", &content);
        
        let tool = ReadFileTool(FilesystemTool::new().with_max_file_size(1000));
        let args = ReadFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            max_size: Some(50),
        };
        let result = tool.call(args).await;
        
        let err = result.unwrap_err();
        assert!(matches!(err, FilesystemError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn test_list_dir_tool_mixed_files_and_dirs() {
        let temp_dir = create_temp_dir();
        create_test_file(&temp_dir, "file1.txt", "content");
        create_test_file(&temp_dir, "file2.txt", "content");
        fs::create_dir(temp_dir.path().join("dir1")).unwrap();
        fs::create_dir(temp_dir.path().join("dir2")).unwrap();
        
        let tool = ListDirTool(FilesystemTool::new());
        let args = ListDirArgs {
            path: temp_dir.path().to_str().unwrap().to_string(),
            recursive: None,
        };
        let result = tool.call(args).await;
        
        let listing = result.unwrap();
        assert_eq!(listing.total_count, 4);
        let files = listing.entries.iter().filter(|e| e.is_file).count();
        let dirs = listing.entries.iter().filter(|e| e.is_dir).count();
        assert_eq!(files, 2);
        assert_eq!(dirs, 2);
    }

    #[tokio::test]
    async fn test_write_file_tool_binary_like_content() {
        let temp_dir = create_temp_dir();
        let file_path = create_test_file_for_write(&temp_dir, "binary.txt");
        let content = "\x00\x01\x02\x03\x7E\x7F";
        
        let tool = WriteFileTool(FilesystemTool::new().with_write_access(true));
        let args = WriteFileArgs {
            path: file_path.to_str().unwrap().to_string(),
            content: content.to_string(),
            create_dirs: None,
        };
        let result = tool.call(args).await;
        
        result.unwrap();
        let read_content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(read_content, content);
    }

    #[test]
    fn test_filesystem_error_display_messages() {
        let err1 = FilesystemError::PermissionDenied("test path".to_string());
        assert_eq!(err1.to_string(), "Permission denied: test path");
        
        let err2 = FilesystemError::PathNotFound("missing.txt".to_string());
        assert_eq!(err2.to_string(), "Path not found: missing.txt");
        
        let err3 = FilesystemError::InvalidPath("bad/path".to_string());
        assert_eq!(err3.to_string(), "Invalid path: bad/path");
    }

    #[tokio::test]
    async fn test_read_file_args_max_size_boundary_values() {
        let args1 = ReadFileArgs {
            path: "/test".to_string(),
            max_size: Some(0),
        };
        assert_eq!(args1.max_size, Some(0));
        
        let args2 = ReadFileArgs {
            path: "/test".to_string(),
            max_size: Some(usize::MAX),
        };
        assert_eq!(args2.max_size, Some(usize::MAX));
    }

    #[test]
    fn test_list_dir_args_recursive_values() {
        let args1 = ListDirArgs {
            path: "/test".to_string(),
            recursive: Some(true),
        };
        assert_eq!(args1.recursive, Some(true));
        
        let args2 = ListDirArgs {
            path: "/test".to_string(),
            recursive: Some(false),
        };
        assert_eq!(args2.recursive, Some(false));
        
        let args3 = ListDirArgs {
            path: "/test".to_string(),
            recursive: None,
        };
        assert_eq!(args3.recursive, None);
    }

    #[test]
    fn test_write_file_args_create_dirs_values() {
        let args1 = WriteFileArgs {
            path: "/test".to_string(),
            content: "test".to_string(),
            create_dirs: Some(true),
        };
        assert_eq!(args1.create_dirs, Some(true));
        
        let args2 = WriteFileArgs {
            path: "/test".to_string(),
            content: "test".to_string(),
            create_dirs: Some(false),
        };
        assert_eq!(args2.create_dirs, Some(false));
        
        let args3 = WriteFileArgs {
            path: "/test".to_string(),
            content: "test".to_string(),
            create_dirs: None,
        };
        assert_eq!(args3.create_dirs, None);
    }
}
