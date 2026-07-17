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

/// Spill `result` to an artifact when it exceeds the configured threshold.
/// Returns the original text when it fits inline or when writing fails.
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
            let truncated = spill.truncate_to_summary(&result);
            match SpilledArtifact::new(&filename, result.len()) {
                Ok(pointer) => pointer.render_with_prefix(&truncated.to_string()),
                Err(_) => result,
            }
        }
        Err(e) => {
            tracing::warn!(
                "Failed to write result artifact for task {}: {}",
                task_id,
                e
            );
            result
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
