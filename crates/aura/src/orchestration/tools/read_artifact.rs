//! Tool for reading result artifacts from execution persistence.

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::orchestration::persistence::ExecutionPersistence;
use crate::scratchpad::storage::{ContentFormat, ScratchpadPathError};
use crate::scratchpad::tools::check_and_record_budget;
use crate::scratchpad::wrapper::build_file_pointer;
use crate::scratchpad::{ContextBudget, ScratchpadStorage, TokenCounter};

/// Fraction of the context window one unbudgeted read may fill.
const INLINE_CAP_WINDOW_FRACTION: f64 = 0.25;

/// Inline cap, in tokens, for a caller with no known context window.
pub const DEFAULT_INLINE_CAP_TOKENS: usize = 32_000;

/// Upper bound on bytes per token across any realistic content and tokenizer.
const MAX_BYTES_PER_TOKEN: usize = 32;

/// Inline cap, in tokens, for a `read_artifact` with no scratchpad budget.
pub fn inline_cap_tokens(context_window: Option<u64>) -> usize {
    context_window
        .map(|w| ((w as f64) * INLINE_CAP_WINDOW_FRACTION) as usize)
        .unwrap_or(DEFAULT_INLINE_CAP_TOKENS)
}

#[derive(Clone)]
struct ReadArtifactScratchpad {
    budget: ContextBudget,
    storage: Arc<ScratchpadStorage>,
}

#[derive(Clone)]
struct InlineCap {
    max_tokens: usize,
    token_counter: Arc<dyn TokenCounter>,
}

impl InlineCap {
    /// Whether `content` fits the cap.
    ///
    /// A token is at least one byte, so content no longer than the cap in
    /// bytes fits without tokenizing; content past `MAX_BYTES_PER_TOKEN`
    /// bytes per allowed token cannot fit and is rejected without tokenizing.
    fn fits(&self, content: &str) -> bool {
        if content.len() <= self.max_tokens {
            return true;
        }
        if content.len() > self.max_tokens.saturating_mul(MAX_BYTES_PER_TOKEN) {
            return false;
        }
        self.token_counter.count_tokens(content) <= self.max_tokens
    }
}

/// Reads full content of a result artifact file.
///
/// Available to both coordinator and workers when execution persistence is enabled.
/// Supports cross-run artifact access via optional `run_id` parameter.
#[derive(Clone)]
pub struct ReadArtifactTool {
    persistence: Arc<Mutex<ExecutionPersistence>>,
    scratchpad: Option<ReadArtifactScratchpad>,
    inline_cap: Option<InlineCap>,
}

impl ReadArtifactTool {
    pub fn new(persistence: Arc<Mutex<ExecutionPersistence>>) -> Self {
        Self {
            persistence,
            scratchpad: None,
            inline_cap: None,
        }
    }

    /// Bound, in tokens, on content returned inline without a scratchpad.
    pub fn with_inline_cap(
        mut self,
        max_tokens: usize,
        token_counter: Arc<dyn TokenCounter>,
    ) -> Self {
        self.inline_cap = Some(InlineCap {
            max_tokens,
            token_counter,
        });
        self
    }

    /// With scratchpad, oversized artifacts are returned as a pointer.
    pub fn with_scratchpad(
        mut self,
        budget: ContextBudget,
        storage: Arc<ScratchpadStorage>,
    ) -> Self {
        self.scratchpad = Some(ReadArtifactScratchpad { budget, storage });
        self
    }

    /// Decide how to surface artifact `content`:
    /// - scratchpad inactive → inline, or a bounded preview when an inline
    ///   cap is set and the content exceeds it;
    /// - fits the budget → inline, recorded against the budget;
    /// - too large → a pointer to the artifact in place (or, if the artifact
    ///   can't be referenced in place, a compact "too large" notice).
    ///
    /// `abs_path` is the artifact's absolute path (already resolved & validated
    /// by the caller); it's used to build the in-place `file=` reference.
    async fn surface_content(&self, filename: &str, abs_path: &Path, content: String) -> String {
        let Some(sp) = &self.scratchpad else {
            return match &self.inline_cap {
                Some(cap) if !cap.fits(&content) => capped_preview(filename, &content, cap),
                _ => content,
            };
        };

        if check_and_record_budget(&sp.budget, &content).is_ok() {
            return content;
        }

        let tokens = sp.budget.count_tokens(&content);
        let line_count = content.lines().count();
        let (format, _) = ContentFormat::detect_and_parse(&content);
        match sp.storage.relative_ref(abs_path).await {
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

/// Head of `content` plus a notice that the artifact exceeded the inline cap.
///
/// The preview is cut by bytes, not lines (a minified JSON artifact can be a
/// single line), at a quarter of the cap; since a token is at least one byte,
/// the preview stays well under the cap in tokens.
fn capped_preview(filename: &str, content: &str, cap: &InlineCap) -> String {
    let mut end = (cap.max_tokens / 4).min(content.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let head = &content[..end];
    let head = match head.rfind('\n') {
        Some(nl) if nl > 0 => &head[..=nl],
        _ => head,
    };
    format!(
        "{head}\n[artifact '{filename}' is too large to return inline ({bytes} bytes, \
         {lines} lines); showing the first {shown} bytes. If more is needed, process it in \
         a narrower task.]",
        bytes = content.len(),
        lines = content.lines().count(),
        shown = head.len(),
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
            description: "Read the content of a result artifact. By default reads from \
                the current run. Supply an optional run_id to read artifacts from a prior run \
                in this session (see session history for available run_id values). A large \
                artifact is returned as a scratchpad pointer to explore in place (with head, \
                grep, slice, etc.) rather than inlined."
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

        let (result, abs_path) = if let Some(ref run_id) = args.run_id {
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
            )
        } else {
            tracing::info!("read_artifact called for: {}", args.filename);
            (
                persistence.read_artifact(&args.filename).await,
                persistence.artifact_path(&args.filename).ok(),
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
                })
            }
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

    // ------------------------------------------------------------------
    // Budget-aware behavior (scratchpad active)
    // ------------------------------------------------------------------

    use crate::scratchpad::TiktokenCounter;

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

        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence)))
            .with_scratchpad(budget.clone(), storage.clone());
        (tool, storage, temp_dir)
    }

    /// A small artifact fits the budget: it is inlined verbatim and recorded as
    /// extracted tokens.
    #[tokio::test]
    async fn test_small_artifact_inlines_and_records_budget() {
        let (tool, storage, _dir) = setup_scratchpad_tool("a short result", 10_000).await;
        let extracted_before = tool
            .scratchpad
            .as_ref()
            .unwrap()
            .budget
            .scratchpad_usage()
            .1;
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
        let extracted = tool
            .scratchpad
            .as_ref()
            .unwrap()
            .budget
            .scratchpad_usage()
            .1;
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
        let head = HeadTool::new(storage, test_budget(10_000));
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
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(run_b)))
            .with_scratchpad(test_budget(50), storage.clone());

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
        let head = HeadTool::new(storage, test_budget(10_000));
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

    /// With no scratchpad attached, a large artifact is returned inline in full.
    #[tokio::test]
    async fn test_no_scratchpad_inlines_large_artifact() {
        let big: String = (0..2_000).map(|i| format!("line {i}\n")).collect();
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        persistence
            .write_result_artifact(0, Some("research"), 1, &big)
            .await
            .unwrap();
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence)));

        let result = tool
            .call(ReadArtifactArgs {
                filename: "task-0-research-iter-1-result.txt".to_string(),
                run_id: None,
            })
            .await
            .unwrap();

        assert!(result.found);
        assert_eq!(
            result.content, big,
            "no-scratchpad path must inline verbatim"
        );
    }

    // ------------------------------------------------------------------
    // Inline cap (no scratchpad budget)
    // ------------------------------------------------------------------

    /// Counter that refuses to tokenize anything larger than `max_len` bytes,
    /// proving oversized content is rejected without a full tokenizer pass.
    struct BoundedCounter {
        max_len: usize,
    }

    impl TokenCounter for BoundedCounter {
        fn count_tokens(&self, text: &str) -> usize {
            assert!(
                text.len() <= self.max_len,
                "tokenizer called on {} bytes",
                text.len()
            );
            TiktokenCounter::default_counter().count_tokens(text)
        }
    }

    async fn setup_capped_tool(content: &str, max_tokens: usize) -> (ReadArtifactTool, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        persistence
            .write_result_artifact(0, Some("research"), 1, content)
            .await
            .unwrap();
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence)))
            .with_inline_cap(max_tokens, Arc::new(TiktokenCounter::default_counter()));
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

    #[tokio::test]
    async fn test_inline_cap_inlines_content_under_cap() {
        let (tool, _dir) = setup_capped_tool("a short result", 1_000).await;
        let result = read_result(&tool).await;
        assert_eq!(result.content, "a short result");
    }

    #[tokio::test]
    async fn test_inline_cap_previews_content_over_cap() {
        let big: String = (0..2_000).map(|i| format!("line number {i}\n")).collect();
        let (tool, _dir) = setup_capped_tool(&big, 500).await;
        let result = read_result(&tool).await;

        assert!(result.found);
        assert!(result.content.starts_with("line number 0\n"));
        assert!(
            result.content.contains("too large to return inline"),
            "expected a cap notice, got: {}",
            result.content
        );
        let tokens = TiktokenCounter::default_counter().count_tokens(&result.content);
        assert!(tokens < 500, "capped output is {tokens} tokens");
    }

    #[tokio::test]
    async fn test_inline_cap_bounds_single_line_preview() {
        let big = format!("[{}]", "{\"k\":\"value\"},".repeat(5_000));
        let (tool, _dir) = setup_capped_tool(&big, 500).await;
        let result = read_result(&tool).await;

        assert!(result.content.starts_with("[{\"k\":"));
        assert!(result.content.contains("too large to return inline"));
        let tokens = TiktokenCounter::default_counter().count_tokens(&result.content);
        assert!(tokens < 500, "capped output is {tokens} tokens");
    }

    #[tokio::test]
    async fn test_inline_cap_rejects_huge_content_without_tokenizing() {
        let big = "x".repeat(2_000_000);
        let temp_dir = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path().join("memory"), None)
            .await
            .unwrap();
        persistence
            .write_result_artifact(0, Some("research"), 1, &big)
            .await
            .unwrap();
        let tool = ReadArtifactTool::new(Arc::new(Mutex::new(persistence)))
            .with_inline_cap(1_000, Arc::new(BoundedCounter { max_len: 100_000 }));

        let result = read_result(&tool).await;
        assert!(result.content.contains("too large to return inline"));
        assert!(result.content.len() < 10_000);
    }

    #[tokio::test]
    async fn test_scratchpad_takes_precedence_over_inline_cap() {
        let big: String = (0..2_000).map(|i| format!("line number {i}\n")).collect();
        let (tool, _storage, _dir) = setup_scratchpad_tool(&big, 50).await;
        let tool = tool.with_inline_cap(500, Arc::new(TiktokenCounter::default_counter()));

        let result = read_result(&tool).await;
        assert!(result.content.contains("head file="));
        assert!(!result.content.contains("too large to return inline"));
    }

    #[tokio::test]
    async fn test_inline_cap_applies_to_cross_run_reads() {
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
            .with_inline_cap(500, Arc::new(TiktokenCounter::default_counter()));

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

    #[test]
    fn test_inline_cap_tokens_scales_with_context_window() {
        assert_eq!(inline_cap_tokens(Some(200_000)), 50_000);
        assert_eq!(inline_cap_tokens(None), DEFAULT_INLINE_CAP_TOKENS);
    }
}
