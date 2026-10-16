//! Tool for reading result artifacts from execution persistence.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::orchestration::persistence::ExecutionPersistence;
use crate::run_context::BoundRun;
use crate::scratchpad::middleware::build_file_pointer;
use crate::scratchpad::storage::{ContentFormat, ScratchpadPathError};
use crate::scratchpad::tools::{check_and_record_budget, check_budget};
use crate::scratchpad::{ContextBudget, ScratchpadStorage};

/// Fraction of the context window one read may fill.
const INLINE_CAP_WINDOW_FRACTION: f64 = 0.25;

/// Bytes of an artifact an unbudgeted read shows.
pub const PREVIEW_BYTES: usize = 8 * 1024;

/// The per-read cap, in tokens, for a run with `context_window` and no
/// scratchpad.
pub fn inline_cap_tokens(context_window: u64) -> usize {
    ((context_window as f64) * INLINE_CAP_WINDOW_FRACTION) as usize
}

/// What bounds the content a read returns inline.
#[derive(Clone)]
enum Surfacing {
    /// No budget.
    Preview,
    /// The bound run's budget, and what stands in for content past it.
    Budgeted {
        run: Arc<BoundRun>,
        fallback: Fallback,
    },
}

/// What stands in for artifact content past the budget.
#[derive(Clone)]
enum Fallback {
    /// An in-place pointer, built against this storage's read root.
    Pointer(Arc<ScratchpadStorage>),
    /// A preview of the artifact's head.
    Preview,
}

/// Reads full content of a result artifact file.
///
/// Available to both coordinator and workers when execution persistence is enabled.
/// Supports cross-run artifact access via optional `run_id` parameter.
#[derive(Clone)]
pub struct ReadArtifactTool {
    persistence: Arc<Mutex<ExecutionPersistence>>,
    surfacing: Surfacing,
}

impl ReadArtifactTool {
    /// A tool with no budget: content under [`PREVIEW_BYTES`] comes back
    /// inline, anything larger as a preview of its head.
    pub fn new(persistence: Arc<Mutex<ExecutionPersistence>>) -> Self {
        Self {
            persistence,
            surfacing: Surfacing::Preview,
        }
    }

    /// Count reads against the bound run's budget; an artifact that does
    /// not fit comes back as a pointer to explore with the scratchpad read
    /// tools.
    pub fn with_scratchpad(mut self, run: Arc<BoundRun>, storage: Arc<ScratchpadStorage>) -> Self {
        self.surfacing = Surfacing::Budgeted {
            run,
            fallback: Fallback::Pointer(storage),
        };
        self
    }

    /// Count reads against the bound run's budget; an artifact that does
    /// not fit comes back as a preview of its head.
    pub fn with_budget(mut self, run: Arc<BoundRun>) -> Self {
        self.surfacing = Surfacing::Budgeted {
            run,
            fallback: Fallback::Preview,
        };
        self
    }

    /// The budget of the run a budgeted tool is bound to.
    #[cfg(test)]
    fn run_budget(&self) -> Option<crate::scratchpad::ContextBudget> {
        match &self.surfacing {
            Surfacing::Budgeted { run, .. } => run.scratchpad_budget(),
            Surfacing::Preview => None,
        }
    }

    fn points_at_scratchpad(&self) -> bool {
        matches!(
            self.surfacing,
            Surfacing::Budgeted {
                fallback: Fallback::Pointer(_),
                ..
            }
        )
    }

    /// Decide how to surface artifact `content`:
    /// - no budget → inline up to [`PREVIEW_BYTES`], else a preview;
    /// - fits the budget → inline, recorded against the budget;
    /// - too large → a pointer to the artifact in place (or, if the artifact
    ///   can't be referenced in place, a compact "too large" notice), or a
    ///   preview sized to the room the budget has left and recorded against it.
    ///
    /// `abs_path` is the artifact's absolute path (already resolved & validated
    /// by the caller); it's used to build the in-place `file=` reference.
    async fn surface_content(&self, filename: &str, abs_path: &Path, content: String) -> String {
        let (run, fallback) = match &self.surfacing {
            Surfacing::Preview => {
                return if content.len() <= PREVIEW_BYTES {
                    content
                } else {
                    capped_preview(filename, &content, PREVIEW_BYTES)
                };
            }
            Surfacing::Budgeted { run, fallback } => (run, fallback),
        };
        // No run bound means no budget to check the artifact against;
        // withhold it rather than inline an unbounded read, as the
        // scratchpad read tools do with `ScratchpadToolError::NoRun`.
        let Some(budget) = run.scratchpad_budget() else {
            tracing::warn!(
                "read_artifact: artifact {} withheld — no run is bound",
                filename
            );
            return format!(
                "[artifact '{filename}' withheld: no run is bound to count it against. \
                 Retry the read_artifact call.]"
            );
        };

        let storage = match fallback {
            Fallback::Pointer(storage) => storage,
            Fallback::Preview => {
                return match check_budget(&budget, &content) {
                    Ok(_) => content,
                    Err(_) => budgeted_preview(filename, &content, &budget),
                };
            }
        };

        let tokens = match check_and_record_budget(&budget, &content) {
            Ok(()) => return content,
            Err(check) => check.tokens(),
        };
        let line_count = content.lines().count();
        let format = ContentFormat::detect(&content);
        match storage.relative_ref(abs_path).await {
            Ok(file_ref) => {
                let headline = format!(
                    "[artifact '{filename}' is too large for the context window \
                     (~{tokens} tokens, {line_count} lines, format={fmt})]",
                    fmt = format.as_str(),
                );
                build_file_pointer(&headline, &file_ref)
            }
            // A validation-time I/O fault (stale mount, permission error) is
            // transient territory: tell the model storage failed and a retry
            // may work, instead of telling it to narrow the task.
            Err(e @ ScratchpadPathError::Io { .. }) => {
                tracing::warn!(
                    "read_artifact: storage error building in-place reference for {}: {}",
                    filename,
                    e
                );
                format!(
                    "[artifact '{filename}' was read (~{tokens} tokens) but a storage \
                     error prevented building an in-place reference: {e}. This may be \
                     transient - retry the read_artifact call.]"
                )
            }
            Err(e) => {
                // Can't reference the artifact in place (e.g. it lives outside
                // the scratchpad read root).
                tracing::warn!(
                    "read_artifact: cannot build in-place reference for {}: {}",
                    filename,
                    e
                );
                format!(
                    "[artifact '{filename}' is too large for the remaining context \
                     window (~{tokens} tokens) and cannot be referenced for in-place \
                     exploration. Narrow the task or summarize upstream so the result \
                     fits.]"
                )
            }
        }
    }
}

/// A preview that fits the room the budget has left, notice included,
/// recorded against the budget.
///
/// The notice is part of what the read returns, so it comes out of the
/// same room as the head; it is sized for a head of the whole room, which
/// can only overstate its length. The complete preview is counted before it
/// is returned. With no room for it, the read returns a one-line note that
/// nothing was shown, which is not recorded, so reads past an exhausted
/// budget do not keep adding to it.
fn budgeted_preview(filename: &str, content: &str, budget: &ContextBudget) -> String {
    let room = budget
        .max_extraction_tokens()
        .unwrap_or(usize::MAX)
        .min(budget.remaining());
    let notice = budget.count_tokens(&preview_notice(filename, content, room));
    let preview = capped_preview(filename, content, room.saturating_sub(notice));
    let tokens = budget.count_tokens(&preview);
    if tokens <= room && budget.try_record_tokens(tokens).is_ok() {
        return preview;
    }
    format!(
        "[artifact '{filename}' not shown: the context window has no room left for it \
         ({bytes} bytes). Narrow the task or have a worker process it.]",
        bytes = content.len(),
    )
}

/// The first `limit` bytes of `content` plus a notice that the rest was
/// left out.
///
/// The cut is by bytes, not lines, since a minified JSON artifact can be a
/// single line; a token is at least one byte, so the head is at most
/// `limit` tokens, and the notice comes on top of it. The head ends at the
/// last newline when that keeps more than half the window, so a long line
/// that follows a short first one is not trimmed back to that first line.
fn capped_preview(filename: &str, content: &str, limit: usize) -> String {
    let mut end = limit.min(content.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let head = &content[..end];
    let head = match head.rfind('\n') {
        Some(nl) if nl > end / 2 => &head[..=nl],
        _ => head,
    };
    format!("{head}{}", preview_notice(filename, content, head.len()))
}

/// The line that follows a preview showing the first `shown` bytes.
fn preview_notice(filename: &str, content: &str, shown: usize) -> String {
    format!(
        "\n[artifact '{filename}' is too large to return inline ({bytes} bytes, \
         {lines} lines); showing the first {shown} bytes. If more is needed, process it in \
         a narrower task.]",
        bytes = content.len(),
        lines = content.lines().count(),
    )
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
    /// Artifact filenames in the current run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub available: Vec<String>,
    /// Number of current-run artifact filenames left out of `available`.
    #[serde(skip_serializing_if = "is_zero")]
    pub available_omitted: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
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

    /// The description tells the model what a large artifact comes back
    /// as, since a pointer is only worth following where the scratchpad
    /// read tools exist.
    async fn definition(&self, _prompt: String) -> ToolDefinition {
        let large = if self.points_at_scratchpad() {
            "A large artifact is returned as a scratchpad pointer to explore in place \
             (with head, grep, slice, etc.) rather than inlined."
        } else {
            "A large artifact is returned as a preview of its start rather than whole; \
             there is no tool to read further, so narrow the task or have a worker \
             process it."
        };
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: format!(
                "Read the content of a result artifact. By default reads from the current \
                 run. Supply an optional run_id to read artifacts from a prior run in this \
                 session (see session history for available run_id values). {large} A \
                 filename that is not found returns found=false with the run's artifact \
                 filenames in `available`."
            ),
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

        let (result, abs_path, available) = if let Some(ref run_id) = args.run_id {
            tracing::info!(
                "read_artifact cross-run: filename={}, run_id={}",
                args.filename,
                run_id
            );
            (
                persistence
                    .read_artifact_cross_run(&args.filename, run_id)
                    .await,
                persistence
                    .artifact_path_cross_run(&args.filename, run_id)
                    .ok(),
                (Vec::new(), 0),
            )
        } else {
            tracing::info!("read_artifact called for: {}", args.filename);
            let result = persistence.read_artifact(&args.filename).await;
            // A miss names what does exist, so the caller can correct the
            // filename instead of guessing again.
            let available = match &result {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => persistence
                    .list_artifacts_capped()
                    .await
                    .unwrap_or_default(),
                _ => (Vec::new(), 0),
            };
            (
                result,
                persistence.artifact_path(&args.filename).ok(),
                available,
            )
        };
        drop(persistence);

        match result {
            Ok(content) => {
                let content = match abs_path {
                    Some(path) => self.surface_content(&args.filename, &path, content).await,
                    // No resolvable path (shouldn't happen on a successful
                    // read) — inline rather than dropping content.
                    None => content,
                };
                Ok(ReadArtifactOutput {
                    found: true,
                    filename: args.filename,
                    content,
                    available: Vec::new(),
                    available_omitted: 0,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReadArtifactOutput {
                found: false,
                filename: args.filename,
                content: String::new(),
                available: available.0,
                available_omitted: available.1,
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
        assert_eq!(result.available, vec!["task-0-research-iter-1-result.txt"]);
        assert_eq!(result.available_omitted, 0);
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

    // ------------------------------------------------------------------
    // Budget-aware behavior (scratchpad active)
    // ------------------------------------------------------------------

    use crate::scratchpad::{ContextBudget, TiktokenCounter, TokenCounter};

    /// A standard 128k-window budget with the given per-call extraction limit.
    fn test_budget(max_extraction_tokens: usize) -> ContextBudget {
        ContextBudget::new(
            128_000,
            0.20,
            0,
            Arc::new(TiktokenCounter::default_counter()),
        )
        .with_max_extraction_tokens(max_extraction_tokens)
    }

    /// Extract the `head file="..."` token from a scratchpad pointer so a test
    /// can feed it straight to a read tool.
    fn file_ref_from_pointer(pointer: &str) -> String {
        let marker = "head file=\"";
        let start = pointer.find(marker).expect("pointer should list head") + marker.len();
        let len = pointer[start..].find('"').unwrap();
        pointer[start..start + len].to_string()
    }

    /// Build a scratchpad-enabled `read_artifact` over a persistence run that
    /// already holds one result artifact. Read root is the run dir, so the
    /// artifact under `{run}/artifacts/` is reachable by the scratchpad tools
    /// in place. Returns the tool plus the scratchpad storage (write dir).
    async fn setup_scratchpad_tool(
        artifact_content: &str,
        max_extraction_tokens: usize,
    ) -> (ReadArtifactTool, Arc<ScratchpadStorage>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        let run_dir = persistence.run_path().to_path_buf();
        let iter_dir = persistence.iteration_path();

        persistence
            .write_result_artifact(0, Some("research"), 1, artifact_content)
            .await
            .unwrap();

        let storage = Arc::new(
            ScratchpadStorage::in_dir(&iter_dir)
                .await
                .unwrap()
                .with_read_root(run_dir),
        );
        let budget = test_budget(max_extraction_tokens);

        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence))).with_scratchpad(
            Arc::new(BoundRun::pinned_budget(budget.clone())),
            storage.clone(),
        );
        (tool, storage, temp_dir)
    }

    /// A small artifact fits the budget: it is inlined verbatim and recorded as
    /// extracted tokens.
    #[tokio::test]
    async fn test_small_artifact_inlines_and_records_budget() {
        let (tool, storage, _dir) = setup_scratchpad_tool("a short result", 10_000).await;
        let extracted_before = tool.run_budget().unwrap().scratchpad_usage().1;
        assert_eq!(extracted_before, 0);

        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: None,
            })
            .await
            .unwrap();

        assert!(result.found);
        assert_eq!(result.content, "a short result");
        // Budget recorded the extraction.
        let extracted = tool.run_budget().unwrap().scratchpad_usage().1;
        assert!(
            extracted > 0,
            "small inline read must record extracted tokens"
        );
        // Nothing was copied into the scratchpad write dir.
        assert!(storage.list_files().await.unwrap().is_empty());
    }

    /// A large artifact (exceeds the per-call limit) is returned as an in-place
    /// pointer naming the artifact and the exploration tools — and is NOT copied
    /// into the scratchpad.
    #[tokio::test]
    async fn test_large_artifact_returns_pointer_without_copying() {
        let big: String = (0..2_000).map(|i| format!("line number {i}\n")).collect();
        let (tool, storage, _dir) = setup_scratchpad_tool(&big, 50).await;

        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: None,
            })
            .await
            .unwrap();

        assert!(result.found);
        assert!(
            result.content.contains("is too large"),
            "expected a too-large pointer, got: {}",
            &result.content[..result.content.len().min(200)]
        );
        // Pointer steers the agent at the scratchpad read tools.
        assert!(result.content.contains("head file="));
        assert!(result.content.contains("grep file="));
        // The reference is the artifact in place (relative path), not a copy.
        assert!(
            result
                .content
                .contains("artifacts/task-0-research-iter-1-result.txt")
        );
        // The raw 2000-line payload did not reach the LLM.
        assert!(result.content.len() < big.len());
        // No file was written under the scratchpad write dir.
        assert!(storage.list_files().await.unwrap().is_empty());
    }

    /// The `file=` token in the pointer is directly usable by a scratchpad read
    /// tool, which reads the artifact in place.
    #[tokio::test]
    async fn test_pointer_file_ref_is_readable_by_head() {
        use crate::scratchpad::HeadTool;
        use crate::scratchpad::tools::HeadArgs;

        let big: String = (0..2_000).map(|i| format!("entry {i} value\n")).collect();
        let (tool, storage, _dir) = setup_scratchpad_tool(&big, 50).await;

        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: None,
            })
            .await
            .unwrap();

        // A read tool resolves the pointer's token to the artifact in place.
        let file_ref = file_ref_from_pointer(&result.content);
        let head = HeadTool::new(
            storage,
            Arc::new(BoundRun::pinned_budget(test_budget(10_000))),
        );
        let head_out = head
            .call(HeadArgs {
                file: file_ref,
                lines: 3,
            })
            .await
            .unwrap();

        assert!(head_out.contains("entry 0 value"));
        assert!(head_out.contains("entry 2 value"));
    }

    /// Cross-run + oversized + pointer usability: reading a large artifact from
    /// a prior run via `run_id` must also return a pointer (not flood context),
    /// and that pointer's `file=` token must read the sibling-run artifact in
    /// place. This guards the read-root scoping — the session dir must cover
    /// sibling run dirs for cross-run pointers to resolve.
    #[tokio::test]
    async fn test_cross_run_large_artifact_returns_readable_pointer() {
        use crate::scratchpad::HeadTool;
        use crate::scratchpad::tools::HeadArgs;

        let temp_dir = TempDir::new().unwrap();
        let memory_dir = temp_dir.path().join("memory");
        let session_id = "session-xrun-large".to_string();

        // Run A writes a large artifact.
        let big: String = (0..2_000).map(|i| format!("xrun line {i}\n")).collect();
        let run_a = ExecutionPersistence::new(&memory_dir, Some(session_id.clone()))
            .await
            .unwrap();
        let run_a_id = run_a.run_id().to_string();
        run_a
            .write_result_artifact(0, Some("sre"), 1, &big)
            .await
            .unwrap();

        // Run B is the current run; its scratchpad read root is the session dir
        // so run A's sibling artifact is reachable in place.
        let run_b = ExecutionPersistence::new(&memory_dir, Some(session_id))
            .await
            .unwrap();
        let read_root = run_b.run_path().parent().unwrap().to_path_buf();
        let iter_dir = run_b.iteration_path();
        let storage = Arc::new(
            ScratchpadStorage::in_dir(&iter_dir)
                .await
                .unwrap()
                .with_read_root(read_root),
        );
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(run_b))).with_scratchpad(
            Arc::new(BoundRun::pinned_budget(test_budget(50))),
            storage.clone(),
        );

        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-sre-iter-1-result.txt".to_string(),
                run_id: Some(run_a_id.clone()),
            })
            .await
            .unwrap();

        // Pointer mode, not a context flood.
        assert!(result.found);
        assert!(
            result.content.contains("is too large"),
            "expected a too-large pointer, got: {}",
            &result.content[..result.content.len().min(200)]
        );
        assert!(result.content.contains("head file="));
        // The reference points at run A's artifact in place (under the session dir).
        assert!(
            result.content.contains(&format!(
                "{run_a_id}/artifacts/task-0-sre-iter-1-result.txt"
            )),
            "pointer should reference the sibling run's artifact in place: {}",
            result.content
        );
        assert!(result.content.len() < big.len());
        // Nothing was copied into the scratchpad write dir.
        assert!(storage.list_files().await.unwrap().is_empty());

        // The token reads the sibling-run artifact in place.
        let file_ref = file_ref_from_pointer(&result.content);
        let head = HeadTool::new(
            storage,
            Arc::new(BoundRun::pinned_budget(test_budget(10_000))),
        );
        let head_out = head
            .call(HeadArgs {
                file: file_ref,
                lines: 3,
            })
            .await
            .unwrap();
        assert!(head_out.contains("xrun line 0"));
        assert!(head_out.contains("xrun line 2"));
    }

    #[tokio::test]
    async fn test_unbudgeted_read_previews_past_preview_bytes() {
        let big: String = (0..3_000).map(|i| format!("line number {i}\n")).collect();
        assert!(big.len() > PREVIEW_BYTES);
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        persistence
            .write_result_artifact(0, Some("research"), 1, &big)
            .await
            .unwrap();
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence)));

        let result = read_result(&tool).await;
        assert!(result.found);
        assert!(result.content.starts_with("line number 0\n"));
        assert!(
            result.content.contains("too large to return inline"),
            "expected a preview notice, got: {}",
            &result.content[result.content.len().saturating_sub(200)..]
        );
        assert!(bytes_shown(&result.content) <= PREVIEW_BYTES);
    }

    // ------------------------------------------------------------------
    // Budgeted reads without a scratchpad
    // ------------------------------------------------------------------

    /// Counter that refuses to tokenize anything larger than `max_len` bytes,
    /// proving oversized content is rejected without a full tokenizer pass.
    struct BoundedCounter {
        max_len: usize,
    }

    impl crate::scratchpad::TokenCounter for BoundedCounter {
        fn count_tokens(&self, text: &str) -> usize {
            assert!(
                text.len() <= self.max_len,
                "tokenizer called on {} bytes",
                text.len()
            );
            TiktokenCounter::default_counter().count_tokens(text)
        }
    }

    async fn setup_budgeted_tool(
        content: &str,
        budget: ContextBudget,
    ) -> (ReadArtifactTool, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        persistence
            .write_result_artifact(0, Some("research"), 1, content)
            .await
            .unwrap();
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence)))
            .with_budget(Arc::new(BoundRun::pinned_budget(budget)));
        (tool, temp_dir)
    }

    async fn read_result(tool: &ReadArtifactTool) -> ReadArtifactOutput {
        tool.call(ReadArtifactArgs {
            filename: "task-0-research-iter-1-result.txt".to_string(),
            run_id: None,
        })
        .await
        .unwrap()
    }

    /// The byte count a preview notice reports.
    fn bytes_shown(content: &str) -> usize {
        let tail = content.rsplit("showing the first ").next().unwrap();
        tail.split(' ').next().unwrap().parse().unwrap()
    }

    #[tokio::test]
    async fn test_budgeted_read_inlines_content_that_fits() {
        let (tool, _dir) = setup_budgeted_tool("a short result", test_budget(1_000)).await;
        assert_eq!(read_result(&tool).await.content, "a short result");
    }

    #[tokio::test]
    async fn test_budgeted_read_previews_content_over_the_cap() {
        let big: String = (0..2_000).map(|i| format!("line number {i}\n")).collect();
        let (tool, _dir) = setup_budgeted_tool(&big, test_budget(500)).await;
        let result = read_result(&tool).await;

        assert!(result.found);
        assert!(result.content.starts_with("line number 0\n"));
        assert!(
            result.content.contains("too large to return inline"),
            "expected a preview notice, got: {}",
            result.content
        );
        assert!(bytes_shown(&result.content) <= 500);
        let tokens = TiktokenCounter::default_counter().count_tokens(&result.content);
        assert!(tokens < 500, "preview is {tokens} tokens");
    }

    #[tokio::test]
    async fn test_budgeted_preview_bounds_a_single_line() {
        let big = format!("[{}]", "{\"k\":\"value\"},".repeat(5_000));
        let (tool, _dir) = setup_budgeted_tool(&big, test_budget(500)).await;
        let result = read_result(&tool).await;

        assert!(result.content.starts_with("[{\"k\":"));
        assert!(result.content.contains("too large to return inline"));
        let tokens = TiktokenCounter::default_counter().count_tokens(&result.content);
        assert!(tokens < 500, "preview is {tokens} tokens");
    }

    #[tokio::test]
    async fn test_budgeted_preview_keeps_the_window_after_a_short_first_line() {
        let big = format!("Result:\n{}", "{\"k\":\"value\"},".repeat(5_000));
        let (tool, _dir) = setup_budgeted_tool(&big, test_budget(500)).await;
        let result = read_result(&tool).await;

        assert!(result.content.starts_with("Result:\n{\"k\""));
        let shown = bytes_shown(&result.content);
        assert!(shown > 250, "preview collapsed to {shown} bytes");
    }

    #[tokio::test]
    async fn test_budgeted_reads_add_up_across_calls() {
        let content: String = (0..60).map(|i| format!("entry number {i}\n")).collect();
        let counter = TiktokenCounter::default_counter();
        let tokens = counter.count_tokens(&content);
        let budget = ContextBudget::new(tokens * 5 / 2, 0.0, 0, Arc::new(counter))
            .with_max_extraction_tokens(tokens + 1);
        let (tool, _dir) = setup_budgeted_tool(&content, budget.clone()).await;

        assert_eq!(read_result(&tool).await.content, content);
        assert_eq!(read_result(&tool).await.content, content);
        let third = read_result(&tool).await.content;
        assert!(
            third.contains("too large to return inline"),
            "the third read must be a preview, got: {third}"
        );
        assert!(
            bytes_shown(&third) <= tokens / 2,
            "the preview is bounded by the room left"
        );
        assert!(
            budget.remaining() < tokens / 2,
            "the preview is counted too"
        );
    }

    /// A budget with `room` tokens left and no per-read cap.
    fn budget_with_room(room: usize) -> ContextBudget {
        ContextBudget::new(
            100_000,
            0.0,
            100_000 - room,
            Arc::new(TiktokenCounter::default_counter()),
        )
    }

    #[tokio::test]
    async fn test_budgeted_preview_fits_its_room_notice_included() {
        let big: String = (0..2_000).map(|i| format!("line number {i}\n")).collect();
        let budget = budget_with_room(200);
        let (tool, _dir) = setup_budgeted_tool(&big, budget.clone()).await;

        let content = read_result(&tool).await.content;
        assert!(content.contains("too large to return inline"), "{content}");
        let tokens = TiktokenCounter::default_counter().count_tokens(&content);
        assert!(tokens <= 200, "the preview is {tokens} tokens");
        assert_eq!(
            budget.remaining(),
            200 - tokens,
            "the whole preview is recorded"
        );
    }

    #[tokio::test]
    async fn test_an_exhausted_budget_stops_growing() {
        let big: String = (0..2_000).map(|i| format!("line number {i}\n")).collect();
        let budget = budget_with_room(10);
        let (tool, _dir) = setup_budgeted_tool(&big, budget.clone()).await;

        for _ in 0..3 {
            let content = read_result(&tool).await.content;
            assert!(content.contains("not shown"), "{content}");
        }
        assert_eq!(
            budget.remaining(),
            10,
            "notes past the budget are not recorded"
        );
    }

    #[tokio::test]
    async fn test_budgeted_read_rejects_huge_content_without_tokenizing() {
        let big = "x".repeat(2_000_000);
        let budget = ContextBudget::new(
            128_000,
            0.20,
            0,
            Arc::new(BoundedCounter { max_len: 100_000 }),
        )
        .with_max_extraction_tokens(1_000);
        let (tool, _dir) = setup_budgeted_tool(&big, budget).await;

        let result = read_result(&tool).await;
        assert!(result.content.contains("too large to return inline"));
        assert!(result.content.len() < 10_000);
    }

    #[tokio::test]
    async fn test_budgeted_read_applies_to_cross_run_reads() {
        let temp_dir = TempDir::new().unwrap();
        let memory_dir = temp_dir.path().join("memory");
        let session_id = "session_cap".to_string();
        let big: String = (0..2_000).map(|i| format!("xrun line {i}\n")).collect();

        let run_a = ExecutionPersistence::new(&memory_dir, Some(session_id.clone()))
            .await
            .unwrap();
        let run_a_id = run_a.run_id().to_string();
        run_a
            .write_result_artifact(0, Some("sre"), 1, &big)
            .await
            .unwrap();

        let run_b = ExecutionPersistence::new(&memory_dir, Some(session_id))
            .await
            .unwrap();
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(run_b)))
            .with_budget(Arc::new(BoundRun::pinned_budget(test_budget(500))));

        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-sre-iter-1-result.txt".to_string(),
                run_id: Some(run_a_id),
            })
            .await
            .unwrap();
        assert!(result.content.starts_with("xrun line 0\n"));
        assert!(result.content.contains("too large to return inline"));
    }

    #[tokio::test]
    async fn test_scratchpad_rejects_huge_content_without_tokenizing() {
        let big = "y\n".repeat(1_000_000);
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        let run_dir = persistence.run_path().to_path_buf();
        let storage = Arc::new(
            ScratchpadStorage::in_dir(&persistence.iteration_path())
                .await
                .unwrap()
                .with_read_root(run_dir),
        );
        persistence
            .write_result_artifact(0, Some("research"), 1, &big)
            .await
            .unwrap();
        let budget = ContextBudget::new(
            128_000,
            0.20,
            0,
            Arc::new(BoundedCounter { max_len: 100_000 }),
        )
        .with_max_extraction_tokens(1_000);
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence)))
            .with_scratchpad(Arc::new(BoundRun::pinned_budget(budget)), storage);

        let result = read_result(&tool).await;
        assert!(result.content.contains("head file="));
    }

    #[tokio::test]
    async fn test_definition_describes_the_fallback() {
        let persistence = Arc::new(Mutex::new(ExecutionPersistence::disabled()));
        let plain = ReadArtifactTool::new(persistence.clone());
        let description = plain.definition(String::new()).await.description;
        assert!(
            description.contains("preview of its start"),
            "{description}"
        );
        assert!(!description.contains("scratchpad pointer"), "{description}");

        let tmp = TempDir::new().unwrap();
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-def")
                .await
                .unwrap(),
        );
        let with_scratchpad = ReadArtifactTool::new(persistence)
            .with_scratchpad(Arc::new(BoundRun::pinned_budget(test_budget(10))), storage);
        let description = with_scratchpad.definition(String::new()).await.description;
        assert!(description.contains("scratchpad pointer"), "{description}");
    }

    #[test]
    fn test_inline_cap_tokens_scales_with_context_window() {
        assert_eq!(inline_cap_tokens(200_000), 50_000);
    }
}
