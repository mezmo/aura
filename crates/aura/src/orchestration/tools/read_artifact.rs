//! Tool for reading result artifacts from execution persistence.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::orchestration::persistence::ExecutionPersistence;

/// Reads full content of a result artifact file.
///
/// Available to both coordinator and workers when execution persistence is enabled.
/// Used to access full results when inline summaries reference an artifact file.
#[derive(Clone)]
pub struct ReadArtifactTool {
    persistence: Arc<Mutex<ExecutionPersistence>>,
}

impl ReadArtifactTool {
    pub fn new(persistence: Arc<Mutex<ExecutionPersistence>>) -> Self {
        Self { persistence }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ReadArtifactArgs {
    /// The artifact filename to read (e.g. "task-0-result.txt").
    pub filename: String,
}

#[derive(Debug, Serialize)]
pub struct ReadArtifactOutput {
    pub found: bool,
    pub filename: String,
    pub content: String,
}

/// Error type for ReadArtifactTool.
#[derive(Debug, thiserror::Error)]
pub enum ReadArtifactError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl Tool for ReadArtifactTool {
    const NAME: &'static str = "read_artifact";

    type Error = ReadArtifactError;
    type Args = ReadArtifactArgs;
    type Output = ReadArtifactOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read the full content of a result artifact. Use this when a task \
                result was too large to include inline and references an artifact file."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "The artifact filename (e.g. 'task-0-result.txt')"
                    }
                },
                "required": ["filename"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::info!("read_artifact called for: {}", args.filename);

        let persistence = self.persistence.lock().await;
        match persistence.read_artifact(&args.filename).await {
            Ok(content) => Ok(ReadArtifactOutput {
                found: true,
                filename: args.filename,
                content,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReadArtifactOutput {
                found: false,
                filename: args.filename,
                content: String::new(),
            }),
            Err(e) => Err(ReadArtifactError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup_tool() -> (ReadArtifactTool, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"))
            .await
            .unwrap();
        let persistence = Arc::new(Mutex::new(persistence));

        // Write a test artifact
        {
            let p = persistence.lock().await;
            p.write_result_artifact(0, "full result content here")
                .await
                .unwrap();
        }

        (ReadArtifactTool::new(persistence), temp_dir)
    }

    #[tokio::test]
    async fn test_read_existing_artifact() {
        let (tool, _dir) = setup_tool().await;
        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-result.txt".to_string(),
            })
            .await
            .unwrap();

        assert!(result.found);
        assert_eq!(result.filename, "task-0-result.txt");
        assert_eq!(result.content, "full result content here");
    }

    #[tokio::test]
    async fn test_read_nonexistent_artifact() {
        let (tool, _dir) = setup_tool().await;
        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-99-result.txt".to_string(),
            })
            .await
            .unwrap();

        assert!(!result.found);
        assert!(result.content.is_empty());
    }

    #[tokio::test]
    async fn test_read_artifact_path_traversal() {
        let (tool, _dir) = setup_tool().await;
        let result = tool
            .call(ReadArtifactArgs {
                filename: "../../../etc/passwd".to_string(),
            })
            .await;

        // Path traversal should fail with InvalidInput
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_artifact_definition() {
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let tool = ReadArtifactTool::new(persistence);
        let def = tool.definition("".to_string()).await;
        assert_eq!(def.name, "read_artifact");
        assert!(def.description.contains("artifact"));
    }
}
