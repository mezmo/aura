use crate::orchestration::bounding::ResultSpillBudget;
use crate::orchestration::context::ContextError;
use crate::orchestration::persistence::ArtifactKind;
use crate::orchestration::persistence::artifacts::ExecutionPersistence;

/// Parse the trailing `[Full result (N chars) saved to artifact: FILE]` footer.
fn parse_trailing_footer(text: &str) -> Option<TrailingFooter> {
    const PREFIX: &str = "[Full result (";
    const INFIX: &str = " chars) saved to artifact: ";
    let start = text.rfind(PREFIX)?;
    let after_prefix = &text[start + PREFIX.len()..];
    let (digits, rest) = after_prefix.split_once(INFIX)?;
    let full_chars: usize = digits.parse().ok()?;
    let filename = rest.trim_end().strip_suffix(']')?;
    let artifact = SpilledArtifact::new(filename, full_chars).ok()?;
    Some(TrailingFooter { start, artifact })
}

/// The single classification point between inline and spilled evidence.
struct TrailingFooter {
    start: usize,
    artifact: SpilledArtifact,
}

/// Pointer to a worker result spilled to an artifact file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpilledArtifact {
    filename: String,
    full_chars: usize,
}

impl SpilledArtifact {
    /// Parse a spilled-result pointer from its artifact filename and the
    /// full result length in characters.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyArtifactFilename`] when `filename` is
    /// empty or only whitespace.
    pub fn new(filename: &str, full_chars: usize) -> Result<Self, ContextError> {
        if filename.trim().is_empty() {
            return Err(ContextError::EmptyArtifactFilename);
        }
        Ok(Self {
            filename: filename.to_owned(),
            full_chars,
        })
    }

    /// Parse the trailing spill footer out of worker-reported text.
    pub fn parse_trailing(text: &str) -> Option<Self> {
        parse_trailing_footer(text).map(|footer| footer.artifact)
    }

    /// Parse the trailing spill footer and return the byte offset where it starts.
    ///
    /// The offset is the index of the `[` in the footer string, used by callers
    /// that need to recover the text that appeared before the footer.
    pub fn parse_trailing_with_offset(text: &str) -> Option<(usize, Self)> {
        parse_trailing_footer(text).map(|footer| (footer.start, footer.artifact))
    }

    /// The artifact filename, readable via `read_artifact`.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Render the pointer together with a stand-in prefix.
    pub fn render_with_prefix(&self, prefix: &str) -> String {
        format!("{prefix}\n\n{self}")
    }
}

impl std::fmt::Display for SpilledArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[Full result ({} chars) saved to artifact: {}]",
            self.full_chars, self.filename
        )
    }
}

/// One artifact inventory line for a completed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    filename: String,
    bytes: u64,
}

impl ArtifactRef {
    /// Parse an artifact inventory reference.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::EmptyArtifactFilename`] when `filename` is
    /// empty or only whitespace.
    pub fn new(filename: &str, bytes: u64) -> Result<Self, ContextError> {
        if filename.trim().is_empty() {
            return Err(ContextError::EmptyArtifactFilename);
        }
        Ok(Self {
            filename: filename.to_owned(),
            bytes,
        })
    }

    /// The artifact filename, readable via `read_artifact`.
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

impl std::fmt::Display for ArtifactRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[Artifact: {} ({} bytes)]", self.filename, self.bytes)
    }
}

/// Marker appended when the artifact is unavailable (the write failed or
/// persistence is disabled), so the unavailability is visible inline
/// instead of silently returning the full body.
const ARTIFACT_WRITE_FAILED_MARKER: &str = "[Artifact write failed; full result unavailable]";

/// Spill `result` to an artifact when it exceeds the configured threshold.
/// Returns the original text when it fits inline. On a successful spill,
/// returns the bounded summary with an artifact-pointer footer. On write
/// failure, returns the bounded inline summary with a failure marker
/// instead of the full body. When persistence is disabled, returns the
/// bounded inline summary with the same failure marker.
pub async fn maybe_spill_result(
    persistence: &ExecutionPersistence,
    spill: &ResultSpillBudget,
    task_id: usize,
    worker_name: Option<&str>,
    result: String,
) -> String {
    if spill.threshold().allows_inline(&result) {
        return result;
    }

    let iteration = persistence.current_iteration();
    match persistence
        .write_result_artifact(task_id, worker_name, iteration, &result)
        .await
    {
        Ok(filename) => {
            let summary = spill.truncate_to_summary(&result);
            match SpilledArtifact::new(&filename, result.len()) {
                Ok(artifact) => artifact.render_with_prefix(&summary.to_string()),
                Err(e) => {
                    tracing::warn!("Result artifact unavailable for task {}: {e}", task_id);
                    format!("{summary}\n\n{ARTIFACT_WRITE_FAILED_MARKER}")
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "Failed to write result artifact for task {}: {}",
                task_id,
                e
            );
            let summary = spill.truncate_to_summary(&result);
            format!("{summary}\n\n{ARTIFACT_WRITE_FAILED_MARKER}")
        }
    }
}

/// Determine artifact kind from the filename convention.
pub fn artifact_kind_from_filename(filename: &str) -> ArtifactKind {
    if filename.ends_with("-result.txt") {
        ArtifactKind::Result
    } else if filename.ends_with("-output.txt") {
        let without_suffix = filename.trim_end_matches("-output.txt");
        let parts: Vec<&str> = without_suffix.split('-').collect();
        let tool_name = parts
            .iter()
            .position(|&p| p == "iter")
            .and_then(|iter_pos| {
                let after_iter = &parts[iter_pos + 2..];
                if after_iter.len() > 1 {
                    Some(after_iter[..after_iter.len() - 1].join("-"))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());
        ArtifactKind::ToolOutput { tool_name }
    } else {
        ArtifactKind::Result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spill_failure_returns_bounded_summary() {
        let budget = ResultSpillBudget::test_budget(10, 5);
        let temp_dir = tempfile::TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(temp_dir.path(), None)
            .await
            .expect("valid temp dir should initialize");

        // Fault injection: place a file where the artifacts directory must be
        // created, forcing `write_result_artifact` to fail.
        let artifacts_path = persistence.run_path().join("artifacts");
        std::fs::write(&artifacts_path, "block").unwrap();

        let result = "this result is longer than the threshold and should be spilled".to_string();
        let got =
            maybe_spill_result(&persistence, &budget, 7, Some("analyst"), result.clone()).await;

        assert!(
            !got.contains(&result),
            "full unbounded result must not be returned inline"
        );
        assert!(
            got.starts_with("this "),
            "bounded summary prefix must be present"
        );
        assert!(
            got.contains(ARTIFACT_WRITE_FAILED_MARKER),
            "failure must be visibly marked"
        );
        let max_len = budget.summary_width().get() + ARTIFACT_WRITE_FAILED_MARKER.len() + 2;
        assert!(
            got.len() <= max_len,
            "result must stay bounded: got {} bytes, max {} bytes",
            got.len(),
            max_len
        );
    }

    #[tokio::test]
    async fn spill_disabled_persistence_returns_bounded_summary() {
        let budget = ResultSpillBudget::test_budget(10, 5);
        let persistence = ExecutionPersistence::disabled();

        let result = "this result is longer than the threshold and should be spilled".to_string();
        let got =
            maybe_spill_result(&persistence, &budget, 7, Some("analyst"), result.clone()).await;

        assert!(
            !got.contains(&result),
            "full unbounded result must not be returned inline"
        );
        assert!(
            got.starts_with("this "),
            "bounded summary prefix must be present"
        );
        assert!(
            got.contains(ARTIFACT_WRITE_FAILED_MARKER),
            "disabled persistence must be visibly marked"
        );
        assert!(
            !got.contains("[Full result ("),
            "disabled persistence must not render an artifact-pointer footer"
        );
        let max_len = budget.summary_width().get() + ARTIFACT_WRITE_FAILED_MARKER.len() + 2;
        assert!(
            got.len() <= max_len,
            "result must stay bounded: got {} bytes, max {} bytes",
            got.len(),
            max_len
        );
    }
}
