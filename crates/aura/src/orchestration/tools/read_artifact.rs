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
/// Supports cross-run artifact access via optional `run_id` parameter.
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
    /// The artifact filename to read (e.g. "task-0-sre-iter-1-result.txt").
    pub filename: String,
    /// Optional run ID for cross-run artifact access. When omitted, reads from
    /// the current run. When provided, resolves against that run's artifacts
    /// within the same session.
    #[serde(default)]
    pub run_id: Option<String>,
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
            description: "Read the full content of a result artifact. By default reads from \
                the current run. Supply an optional run_id to read artifacts from a prior run \
                in this session (see session history for available run_id values)."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "The artifact filename (e.g. 'task-0-sre-iter-1-result.txt')"
                    },
                    "run_id": {
                        "type": "string",
                        "description": "Run ID for cross-run artifact access. Omit to read from the current run."
                    }
                },
                "required": ["filename"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let persistence = self.persistence.lock().await;

        let result = if let Some(ref run_id) = args.run_id {
            tracing::info!(
                "read_artifact cross-run: filename={}, run_id={}",
                args.filename,
                run_id
            );
            persistence
                .read_artifact_cross_run(&args.filename, run_id)
                .await
        } else {
            tracing::info!("read_artifact called for: {}", args.filename);
            persistence.read_artifact(&args.filename).await
        };

        match result {
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
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        let persistence = Arc::new(Mutex::new(persistence));

        // Write a test artifact
        {
            let p = persistence.lock().await;
            p.write_result_artifact(0, Some("research"), 1, "full result content here")
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
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: None,
            })
            .await
            .unwrap();

        assert!(result.found);
        assert_eq!(result.filename, "task-0-research-iter-1-result.txt");
        assert_eq!(result.content, "full result content here");
    }

    #[tokio::test]
    async fn test_read_nonexistent_artifact() {
        let (tool, _dir) = setup_tool().await;
        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-99-default-iter-1-result.txt".to_string(),
                run_id: None,
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
                run_id: None,
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
        assert!(def.description.contains("run_id"));

        let params = def.parameters;
        let props = params.get("properties").unwrap();
        assert!(props.get("run_id").is_some());
    }

    #[tokio::test]
    async fn test_read_cross_run_artifact() {
        let temp_dir = TempDir::new().unwrap();
        let memory_dir = temp_dir.path().join("memory");
        let session_id = "session_cross_run".to_string();

        // Create run A and write an artifact
        let run_a = ExecutionPersistence::new(&memory_dir, Some(session_id.clone()))
            .await
            .unwrap();
        let run_a_id = run_a.run_id().to_string();
        run_a
            .write_result_artifact(0, Some("sre"), 1, "cross-run artifact content")
            .await
            .unwrap();

        // Create run B (the "current" run)
        let run_b = ExecutionPersistence::new(&memory_dir, Some(session_id))
            .await
            .unwrap();
        let persistence = Arc::new(Mutex::new(run_b));
        let tool = ReadArtifactTool::new(persistence);

        // Read run A's artifact from run B via cross-run
        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-sre-iter-1-result.txt".to_string(),
                run_id: Some(run_a_id),
            })
            .await
            .unwrap();

        assert!(result.found);
        assert_eq!(result.content, "cross-run artifact content");
    }

    #[tokio::test]
    async fn test_read_cross_run_path_traversal() {
        let (tool, _dir) = setup_tool().await;
        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: Some("../../etc".to_string()),
            })
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_cross_run_nonexistent_run() {
        let (tool, _dir) = setup_tool().await;
        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: Some("nonexistent-run-id".to_string()),
            })
            .await
            .unwrap();

        assert!(!result.found);
    }

    #[tokio::test]
    async fn test_read_cross_run_no_session() {
        let temp_dir = TempDir::new().unwrap();
        let memory_dir = temp_dir.path().join("memory");

        // Create run A (flat layout, no session_id) and write artifact
        let run_a = ExecutionPersistence::new(&memory_dir, None).await.unwrap();
        let run_a_id = run_a.run_id().to_string();
        run_a
            .write_result_artifact(0, Some("default"), 1, "flat layout artifact")
            .await
            .unwrap();

        // Create run B (flat layout)
        let run_b = ExecutionPersistence::new(&memory_dir, None).await.unwrap();
        let persistence = Arc::new(Mutex::new(run_b));
        let tool = ReadArtifactTool::new(persistence);

        // Cross-run read still works via parent directory
        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-default-iter-1-result.txt".to_string(),
                run_id: Some(run_a_id),
            })
            .await
            .unwrap();

        assert!(result.found);
        assert_eq!(result.content, "flat layout artifact");
    }
}
