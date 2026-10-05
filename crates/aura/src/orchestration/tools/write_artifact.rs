//! Tool for the coordinator to write content to an artifact file.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::orchestration::persistence::ExecutionPersistence;

/// Coordinator tool that saves content as a run artifact; the write-side
/// counterpart to [`ReadArtifactTool`](super::ReadArtifactTool).
#[derive(Clone)]
pub struct WriteArtifactTool {
    persistence: Arc<Mutex<ExecutionPersistence>>,
}

impl WriteArtifactTool {
    pub fn new(persistence: Arc<Mutex<ExecutionPersistence>>) -> Self {
        Self { persistence }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WriteArtifactArgs {
    /// Requested artifact name (e.g. "runbook-draft.md").
    pub filename: String,
    /// The full content to write.
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct WriteArtifactOutput {
    pub written: bool,
    /// Stored artifact filename.
    pub filename: String,
    /// Whether the write overwrote an existing artifact.
    pub replaced: bool,
    pub chars: usize,
    /// Why nothing was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Error type for WriteArtifactTool.
#[derive(Debug, thiserror::Error)]
pub enum WriteArtifactError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl Tool for WriteArtifactTool {
    const NAME: &'static str = "write_artifact";

    type Error = WriteArtifactError;
    type Args = WriteArtifactArgs;
    type Output = WriteArtifactOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Save content to an artifact file in the current run so workers can \
                load it with read_artifact. Use this to hand a worker a document or a set of \
                exact values that is too large or too precise to embed in a task description: \
                write it once, then name the returned filename in the task. The filename is \
                normalized to 'coordinator-<name>'; always reference the filename this tool \
                returns. Writing the same name again replaces the artifact."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Short descriptive name with an optional extension (e.g. 'runbook-draft.md')"
                    },
                    "content": {
                        "type": "string",
                        "description": "The full content to save"
                    }
                },
                "required": ["filename", "content"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let chars = args.content.chars().count();
        let result = self
            .persistence
            .lock()
            .await
            .write_coordinator_artifact(&args.filename, &args.content)
            .await;

        match result {
            Ok((filename, replaced)) => Ok(WriteArtifactOutput {
                written: true,
                filename,
                replaced,
                chars,
                error: None,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                tracing::warn!("write_artifact called with persistence disabled");
                Ok(WriteArtifactOutput {
                    written: false,
                    filename: String::new(),
                    replaced: false,
                    chars,
                    error: Some(
                        "Artifacts are unavailable (execution persistence is disabled). \
                         Embed the content in the task description instead."
                            .to_string(),
                    ),
                })
            }
            Err(e) => Err(WriteArtifactError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::tools::ReadArtifactTool;
    use crate::orchestration::tools::read_artifact::ReadArtifactArgs;
    use tempfile::TempDir;

    async fn setup() -> (Arc<Mutex<ExecutionPersistence>>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        (Arc::new(Mutex::new(persistence)), temp_dir)
    }

    #[tokio::test]
    async fn test_write_then_read_artifact() {
        let (persistence, _dir) = setup().await;
        let writer = WriteArtifactTool::new(persistence.clone());
        let reader = ReadArtifactTool::new(persistence);

        let written = writer
            .call(WriteArtifactArgs {
                filename: "runbook-draft.md".to_string(),
                content: "# Draft\n\nexact `query{a=\"b\"}`\n".to_string(),
            })
            .await
            .unwrap();
        assert!(written.written);
        assert_eq!(written.filename, "coordinator-runbook-draft.md");
        assert!(!written.replaced);
        assert!(written.error.is_none());

        let read = reader
            .call(ReadArtifactArgs {
                filename: written.filename,
                run_id: None,
            })
            .await
            .unwrap();
        assert!(read.found);
        assert_eq!(read.content, "# Draft\n\nexact `query{a=\"b\"}`\n");
    }

    #[tokio::test]
    async fn test_rewrite_reports_replaced() {
        let (persistence, _dir) = setup().await;
        let writer = WriteArtifactTool::new(persistence);
        for (content, expect_replaced) in [("v1", false), ("v2", true)] {
            let out = writer
                .call(WriteArtifactArgs {
                    filename: "draft".to_string(),
                    content: content.to_string(),
                })
                .await
                .unwrap();
            assert_eq!(out.filename, "coordinator-draft.txt");
            assert_eq!(out.replaced, expect_replaced);
        }
    }

    #[tokio::test]
    async fn test_path_traversal_is_contained() {
        let (persistence, _dir) = setup().await;
        let artifacts_dir = persistence.lock().await.artifacts_path();
        let writer = WriteArtifactTool::new(persistence);

        let out = writer
            .call(WriteArtifactArgs {
                filename: "../../escape.md".to_string(),
                content: "x".to_string(),
            })
            .await
            .unwrap();
        assert!(out.written);
        assert_eq!(out.filename, "coordinator-escape.md");
        assert!(artifacts_dir.join(&out.filename).exists());
    }

    #[tokio::test]
    async fn test_disabled_persistence_reports_not_written() {
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let writer = WriteArtifactTool::new(persistence);
        let out = writer
            .call(WriteArtifactArgs {
                filename: "draft.md".to_string(),
                content: "content".to_string(),
            })
            .await
            .unwrap();
        assert!(!out.written);
        assert!(out.filename.is_empty());
        assert!(out.error.unwrap().contains("persistence is disabled"));
    }

    #[tokio::test]
    async fn test_write_artifact_definition() {
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let def = WriteArtifactTool::new(persistence)
            .definition(String::new())
            .await;
        assert_eq!(def.name, "write_artifact");
        assert!(def.description.contains("read_artifact"));
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "filename"));
        assert!(required.iter().any(|v| v == "content"));
    }
}
