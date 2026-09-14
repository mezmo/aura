//! `edit`: an exact-match replacement on a stored file, saved as a new file.
//!
//! Pairs with by-reference arguments: the model changes a stored file by
//! sending only the changed text, then passes the new file's name to a
//! `<field>_file` argument instead of retyping the whole file. The source
//! file is never modified, so references to it stay valid.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::arg_reference::ReferenceResolver;
use super::storage::ScratchpadStorage;
use super::tools::ScratchpadToolError;
use crate::mcp::MAX_RAW_PAYLOAD_BYTES;

/// Lines of context shown around the first replacement.
const CONTEXT_LINES: usize = 2;
/// Most lines shown in the excerpt of the edited region.
const MAX_EXCERPT_LINES: usize = 20;
/// Most characters shown per excerpt line.
const MAX_EXCERPT_LINE_CHARS: usize = 200;
/// Most match locations listed when `old` is ambiguous.
const MAX_LISTED_MATCHES: usize = 10;

#[derive(Clone)]
pub struct EditTool {
    storage: Arc<ScratchpadStorage>,
    resolver: ReferenceResolver,
}

impl EditTool {
    pub fn new(storage: Arc<ScratchpadStorage>, resolver: ReferenceResolver) -> Self {
        Self { storage, resolver }
    }

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_string(),
            description: "Replace exact text in a stored file and save the result as a new \
                          file, so a changed file can be sent through a `<field>_file` \
                          argument without retyping it. `old` must match the file exactly \
                          (whitespace and line breaks included) and occur exactly once, unless \
                          `replace_all` is set. The original file is left unchanged; the result \
                          names the new file — pass that name to `<field>_file`, or edit it again."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Scratchpad file or run artifact to edit — the names \
                                        `<field>_file` takes (prefer an intercepted file's \
                                        `.raw` copy)"
                    },
                    "old": {
                        "type": "string",
                        "description": "Exact text to replace, copied from the file without \
                                        line-number prefixes"
                    },
                    "new": {
                        "type": "string",
                        "description": "Replacement text (empty to delete `old`)"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence of `old` instead of requiring \
                                        exactly one (default false)"
                    }
                },
                "required": ["file", "old", "new"],
                "additionalProperties": false
            }),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct EditArgs {
    pub file: String,
    pub old: String,
    pub new: String,
    #[serde(default)]
    pub replace_all: bool,
}

impl Tool for EditTool {
    const NAME: &'static str = "edit";
    type Error = ScratchpadToolError;
    type Args = EditArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        Self::tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        tracing::debug!(
            "scratchpad edit: file={}, replace_all={}",
            args.file,
            args.replace_all
        );
        if args.old.is_empty() {
            return Err(ScratchpadToolError::InvalidArg(
                "`old` must not be empty".to_string(),
            ));
        }
        if args.old == args.new {
            return Err(ScratchpadToolError::InvalidArg(
                "`old` and `new` are identical; there is nothing to change".to_string(),
            ));
        }

        let content = self.resolver.read(&args.file).await.map_err(|reason| {
            ScratchpadToolError::InvalidArg(format!("cannot read '{}': {reason}", args.file))
        })?;
        let edit = apply_edit(&content, &args.old, &args.new, args.replace_all)
            .map_err(ScratchpadToolError::InvalidArg)?;
        // The result is only useful as a reference, and references refuse
        // larger files.
        if edit.content.len() > MAX_RAW_PAYLOAD_BYTES {
            return Err(ScratchpadToolError::InvalidArg(format!(
                "the edited file would be {} bytes; stored files are limited to \
                 {MAX_RAW_PAYLOAD_BYTES} bytes",
                edit.content.len()
            )));
        }

        let name = edited_file_name(&args.file, &edit.content);
        self.storage.write_verbatim(&name, &edit.content).await?;

        let replacements = match edit.replacements {
            1 => "1 replacement".to_string(),
            n => format!("{n} replacements"),
        };
        Ok(format!(
            "[edit: saved '{name}' ({replacements}, {lines} lines); '{file}' is unchanged. \
             Pass file=\"{name}\" to a `<field>_file` argument to send it, or edit \
             '{name}' again.]\n\n{excerpt}",
            lines = edit.content.lines().count(),
            file = args.file,
            excerpt = excerpt(&edit.content, edit.first_at, &args.new),
        ))
    }
}

/// The result of applying one edit.
struct Edit {
    content: String,
    replacements: usize,
    /// Byte offset of the first replacement (the same in old and new content).
    first_at: usize,
}

/// Replace `old` with `new` in `content`: exactly one occurrence, or every
/// occurrence with `replace_all`. The error is a model-facing reason.
fn apply_edit(content: &str, old: &str, new: &str, replace_all: bool) -> Result<Edit, String> {
    let positions: Vec<usize> = content.match_indices(old).map(|(at, _)| at).collect();
    let Some(&first_at) = positions.first() else {
        return Err(no_match_reason(content, old));
    };
    if positions.len() > 1 && !replace_all {
        let lines: Vec<String> = positions
            .iter()
            .take(MAX_LISTED_MATCHES)
            .map(|&at| line_of(content, at).to_string())
            .collect();
        let more = if positions.len() > MAX_LISTED_MATCHES {
            ", …"
        } else {
            ""
        };
        return Err(format!(
            "`old` occurs {} times (at lines {}{more}); include more surrounding text so it \
             matches exactly once, or set replace_all to change every occurrence",
            positions.len(),
            lines.join(", "),
        ));
    }

    let (content, replacements) = if replace_all {
        (content.replace(old, new), positions.len())
    } else {
        (content.replacen(old, new, 1), 1)
    };
    Ok(Edit {
        content,
        replacements,
        first_at,
    })
}

fn no_match_reason(content: &str, old: &str) -> String {
    let mut reason = "`old` does not occur in the file. It must match exactly, including \
                      whitespace and line breaks; copy it from `grep` or `slice` output \
                      without the line-number prefix."
        .to_string();
    if content.contains("\r\n") && old.contains('\n') && !old.contains("\r\n") {
        reason.push_str(" The file uses CRLF (\\r\\n) line endings.");
    }
    reason
}

/// 1-indexed line number of byte offset `at`.
fn line_of(content: &str, at: usize) -> usize {
    content[..at].matches('\n').count() + 1
}

/// Numbered lines of the edited `content` around the replacement text `new`
/// that starts at byte `at`, so the model can check the edit without reading
/// the whole file.
fn excerpt(content: &str, at: usize, new: &str) -> String {
    let first = line_of(content, at);
    let last = first + new.matches('\n').count();
    let start = first.saturating_sub(CONTEXT_LINES).max(1);
    let end = (last + CONTEXT_LINES).min(start + MAX_EXCERPT_LINES - 1);
    let lines: Vec<String> = content
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(end + 1 - start)
        .map(|(i, line)| {
            let shown: String = line.chars().take(MAX_EXCERPT_LINE_CHARS).collect();
            let cut = if shown.len() < line.len() { "…" } else { "" };
            format!("{:>6}\t{shown}{cut}", i + 1)
        })
        .collect();
    let span = if first == last {
        format!("line {first}")
    } else {
        format!("lines {first}–{last}")
    };
    format!(
        "Edited region ({span} of the new file):\n{}",
        lines.join("\n")
    )
}

/// Name for an edited copy of `source`: its base name with an
/// `.edit-<hash8>` segment before the extension, hashed on the new content so
/// identical edits produce the same file. An existing `.edit-<hash8>` segment
/// is replaced rather than stacked.
fn edited_file_name(source: &str, content: &str) -> String {
    let base = Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let (stem, extension) = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (base.clone(), String::new()),
    };
    let stem = match stem.rsplit_once(".edit-") {
        Some((head, hash)) if hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            head.to_string()
        }
        _ => stem,
    };

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    format!("{stem}.edit-{}{extension}", &hash[..8]).replace(['/', '\\', ':', ' '], "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const RUNBOOK: &str = "# Runbook\n\n- step one\n- step two\n- step three\n";

    async fn tool_with(
        tmp: &TempDir,
        name: &str,
        content: &str,
    ) -> (EditTool, Arc<ScratchpadStorage>) {
        let storage = Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-edit")
                .await
                .unwrap(),
        );
        tokio::fs::write(storage.dir().join(name), content)
            .await
            .unwrap();
        let resolver = ReferenceResolver::new(storage.clone(), None);
        (EditTool::new(storage.clone(), resolver), storage)
    }

    fn args(file: &str, old: &str, new: &str) -> EditArgs {
        EditArgs {
            file: file.to_string(),
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
        }
    }

    /// The saved file's name, as reported in the tool output.
    fn saved_name(output: &str) -> &str {
        let start = output.find("saved '").expect("output names the file") + "saved '".len();
        let len = output[start..].find('\'').unwrap();
        &output[start..start + len]
    }

    #[tokio::test]
    async fn edit_saves_a_new_file_and_leaves_the_source_alone() {
        let tmp = TempDir::new().unwrap();
        let (tool, storage) = tool_with(&tmp, "runbook.raw.md", RUNBOOK).await;

        let output = tool
            .call(args(
                "runbook.raw.md",
                "- step two\n",
                "- step 2 (updated)\n",
            ))
            .await
            .unwrap();

        let name = saved_name(&output);
        assert!(
            name.starts_with("runbook.raw.edit-") && name.ends_with(".md"),
            "{name}"
        );
        assert!(output.contains("1 replacement"), "{output}");
        assert!(
            output.contains("- step 2 (updated)"),
            "excerpt shows the change: {output}"
        );
        let edited = tokio::fs::read_to_string(storage.dir().join(name))
            .await
            .unwrap();
        assert_eq!(
            edited,
            "# Runbook\n\n- step one\n- step 2 (updated)\n- step three\n"
        );
        let source = tokio::fs::read_to_string(storage.dir().join("runbook.raw.md"))
            .await
            .unwrap();
        assert_eq!(source, RUNBOOK, "the source must stay referenceable as-is");
    }

    #[tokio::test]
    async fn edit_of_an_edit_keeps_a_single_edit_segment() {
        let tmp = TempDir::new().unwrap();
        let (tool, storage) = tool_with(&tmp, "runbook.raw.md", RUNBOOK).await;

        let first = tool
            .call(args("runbook.raw.md", "step one", "step 1"))
            .await
            .unwrap();
        let first = saved_name(&first).to_string();
        let second = tool.call(args(&first, "step two", "step 2")).await.unwrap();
        let second = saved_name(&second);

        assert_eq!(second.matches(".edit-").count(), 1, "{second}");
        let edited = tokio::fs::read_to_string(storage.dir().join(second))
            .await
            .unwrap();
        assert_eq!(edited, "# Runbook\n\n- step 1\n- step 2\n- step three\n");
    }

    #[tokio::test]
    async fn edit_rejects_text_that_does_not_occur() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = tool_with(&tmp, "f.md", RUNBOOK).await;
        let err = tool
            .call(args("f.md", "step four", "step 4"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not occur"), "{err}");
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_text_and_lists_where_it_occurs() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = tool_with(&tmp, "f.md", RUNBOOK).await;
        let err = tool
            .call(args("f.md", "- step", "* step"))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("occurs 3 times (at lines 3, 4, 5)"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn replace_all_changes_every_occurrence() {
        let tmp = TempDir::new().unwrap();
        let (tool, storage) = tool_with(&tmp, "f.md", RUNBOOK).await;
        let output = tool
            .call(EditArgs {
                replace_all: true,
                ..args("f.md", "- step", "* step")
            })
            .await
            .unwrap();
        assert!(output.contains("3 replacements"), "{output}");
        let edited = tokio::fs::read_to_string(storage.dir().join(saved_name(&output)))
            .await
            .unwrap();
        assert_eq!(
            edited,
            "# Runbook\n\n* step one\n* step two\n* step three\n"
        );
    }

    #[tokio::test]
    async fn edit_rejects_empty_and_identical_text() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = tool_with(&tmp, "f.md", RUNBOOK).await;
        let empty = tool.call(args("f.md", "", "x")).await.unwrap_err();
        assert!(empty.to_string().contains("must not be empty"), "{empty}");
        let same = tool
            .call(args("f.md", "step one", "step one"))
            .await
            .unwrap_err();
        assert!(same.to_string().contains("identical"), "{same}");
    }

    #[tokio::test]
    async fn edit_hints_at_crlf_line_endings() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = tool_with(&tmp, "f.txt", "one\r\ntwo\r\n").await;
        let err = tool
            .call(args("f.txt", "one\ntwo", "1\n2"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CRLF"), "{err}");
    }

    #[tokio::test]
    async fn edit_rejects_a_missing_file() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = tool_with(&tmp, "f.md", RUNBOOK).await;
        let err = tool.call(args("nope.md", "a", "b")).await.unwrap_err();
        assert!(err.to_string().contains("cannot read 'nope.md'"), "{err}");
    }

    #[tokio::test]
    async fn edit_rejects_a_result_over_the_size_limit() {
        let tmp = TempDir::new().unwrap();
        let content = format!("{}y", "x".repeat(MAX_RAW_PAYLOAD_BYTES - 1));
        let (tool, _) = tool_with(&tmp, "big.txt", &content).await;
        let err = tool.call(args("big.txt", "y", "zz")).await.unwrap_err();
        assert!(err.to_string().contains("limited to"), "{err}");
    }

    #[test]
    fn edited_file_name_uses_the_base_name_and_hashes_the_content() {
        let a = edited_file_name("../../artifacts/task-0-w-iter-1-t-0-output.txt", "new");
        assert!(a.starts_with("task-0-w-iter-1-t-0-output.edit-"), "{a}");
        assert!(a.ends_with(".txt"), "{a}");
        assert_eq!(
            a,
            edited_file_name("../../artifacts/task-0-w-iter-1-t-0-output.txt", "new"),
            "identical edits land in the same file"
        );
        assert_ne!(
            a,
            edited_file_name("../../artifacts/task-0-w-iter-1-t-0-output.txt", "other")
        );
        assert!(edited_file_name("noext", "x").starts_with("noext.edit-"));
    }

    #[tokio::test]
    async fn tool_definition_matches_trait_definition() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = tool_with(&tmp, "f.md", RUNBOOK).await;
        assert_eq!(
            EditTool::tool_definition(),
            tool.definition(String::new()).await
        );
    }
}
