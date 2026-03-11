//! On-demand skill loading tool.
//!
//! Provides a `load_skill` tool that the LLM can call to retrieve detailed
//! instructions for a specific workflow. Skills follow the Agent Skills
//! specification (<https://agentskills.io/specification>).
//!
//! Skills are discovered from directories containing a `SKILL.md` file with
//! YAML frontmatter. Content is read from disk on demand, keeping the base
//! system prompt small.

use crate::config::SkillConfig;
use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::fs;

/// Size threshold (in bytes) above which a warning is logged.
/// Skills are still loaded in full — the operator controls their own content
/// and the LLM's context window is the natural limit.
const SKILL_SIZE_WARN_THRESHOLD: usize = 524_288; // 512 KB

/// Tool that loads skill content on demand from disk.
///
/// The LLM sees a catalog of available skills in the tool description
/// and calls `load_skill("skill_name")` when it needs detailed instructions.
#[derive(Debug, Clone)]
pub struct LoadSkillTool {
    skills: Vec<SkillConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("Unknown skill: '{0}'. Use one of the available skill names.")]
    UnknownSkill(String),

    #[error("Failed to read skill file: {0}")]
    IoError(#[from] std::io::Error),
}

#[derive(Deserialize, Serialize)]
pub struct LoadSkillArgs {
    /// Name of the skill to load
    pub name: String,
}

/// Strip YAML frontmatter from content, returning only the body.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }

    let after_first = &trimmed[3..];
    match after_first.find("---") {
        Some(pos) => {
            let body = &after_first[pos + 3..];
            body.strip_prefix('\n').unwrap_or(body)
        }
        None => content,
    }
}

impl LoadSkillTool {
    /// Create a new LoadSkillTool from discovered skill configs.
    ///
    /// Each `SkillConfig` contains an absolute path to the skill directory.
    pub fn new(skills: &[SkillConfig]) -> Self {
        Self {
            skills: skills.to_vec(),
        }
    }

    /// Build the tool description listing all available skills.
    fn build_description(&self) -> String {
        let mut desc =
            String::from("Load detailed instructions for a specific skill. Available skills:\n");
        for skill in &self.skills {
            desc.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        desc
    }
}

impl Tool for LoadSkillTool {
    const NAME: &'static str = "load_skill";
    type Error = SkillError;
    type Args = LoadSkillArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.build_description(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to load"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let skill = self
            .skills
            .iter()
            .find(|s| s.name == args.name)
            .ok_or_else(|| SkillError::UnknownSkill(args.name.clone()))?;

        let skill_file = skill.path.join("SKILL.md");
        tracing::info!("Loading skill '{}' from {:?}", skill.name, skill_file);

        let content = fs::read_to_string(&skill_file).await?;
        let body = strip_frontmatter(&content);
        let mut result = body.to_string();

        tracing::info!(
            "Skill '{}' SKILL.md loaded ({} bytes)",
            skill.name,
            result.len()
        );

        // Load additional files from optional directories (references/, scripts/, assets/)
        // if they exist, per the Agent Skills spec's progressive disclosure model.
        // The SKILL.md body can reference these files; we include references/ automatically
        // since they contain documentation the LLM may need.
        let references_dir = skill.path.join("references");
        if references_dir.is_dir()
            && let Ok(mut entries) = tokio::fs::read_dir(&references_dir).await
        {
            // Collect and sort entries by filename for deterministic ordering
            let mut ref_paths = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_file() {
                    ref_paths.push(path);
                }
            }
            ref_paths.sort();

            for path in ref_paths {
                match fs::read_to_string(&path).await {
                    Ok(file_content) => {
                        let file_name =
                            path.file_name().unwrap_or_default().to_string_lossy();
                        tracing::info!(
                            "Skill '{}' reference '{}' loaded ({} bytes)",
                            skill.name,
                            file_name,
                            file_content.len()
                        );
                        result.push_str("\n\n");
                        result.push_str(&file_content);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Skill '{}': failed to read reference file {:?}: {}",
                            skill.name,
                            path,
                            e
                        );
                    }
                }
            }
        }

        if result.len() > SKILL_SIZE_WARN_THRESHOLD {
            tracing::warn!(
                "Skill '{}' content is large ({} bytes). This may consume significant \
                 LLM context window. Consider splitting into smaller skills if possible.",
                skill.name,
                result.len()
            );
        }

        tracing::info!(
            "Skill '{}' fully loaded ({} bytes total)",
            skill.name,
            result.len()
        );

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_skill_dir(
        dir: &std::path::Path,
        name: &str,
        frontmatter: &str,
        body: &str,
    ) -> PathBuf {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut file = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        write!(file, "{frontmatter}{body}").unwrap();
        skill_dir
    }

    fn make_skill_configs(dir: &std::path::Path) -> Vec<SkillConfig> {
        let path1 = make_skill_dir(
            dir,
            "test-skill",
            "---\nname: test-skill\ndescription: A test skill\n---\n",
            "# Test Skill\nThis is test content.",
        );
        let path2 = make_skill_dir(
            dir,
            "another-skill",
            "---\nname: another-skill\ndescription: Another test skill\n---\n",
            "# Another Skill\nMore content.",
        );
        vec![
            SkillConfig {
                name: "test-skill".to_string(),
                description: "A test skill".to_string(),
                path: path1,
            },
            SkillConfig {
                name: "another-skill".to_string(),
                description: "Another test skill".to_string(),
                path: path2,
            },
        ]
    }

    #[tokio::test]
    async fn test_load_skill_success() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_configs(dir.path());
        let tool = LoadSkillTool::new(&configs);
        let result = tool
            .call(LoadSkillArgs {
                name: "test-skill".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(result, "# Test Skill\nThis is test content.");
    }

    #[tokio::test]
    async fn test_load_skill_unknown() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_configs(dir.path());
        let tool = LoadSkillTool::new(&configs);
        let result = tool
            .call(LoadSkillArgs {
                name: "nonexistent".to_string(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown skill"));
    }

    #[tokio::test]
    async fn test_load_skill_file_not_found() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("missing-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let configs = vec![SkillConfig {
            name: "missing-skill".to_string(),
            description: "Missing".to_string(),
            path: skill_dir,
        }];
        let tool = LoadSkillTool::new(&configs);
        let result = tool
            .call(LoadSkillArgs {
                name: "missing-skill".to_string(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to read"));
    }

    #[tokio::test]
    async fn test_definition_lists_all_skills() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_configs(dir.path());
        let tool = LoadSkillTool::new(&configs);
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "load_skill");
        assert!(def.description.contains("test-skill"));
        assert!(def.description.contains("another-skill"));
        assert!(def.description.contains("A test skill"));
        assert!(def.description.contains("Another test skill"));
    }

    #[test]
    fn test_strip_frontmatter_basic() {
        let content = "---\nname: test\ndescription: test\n---\n# Body\nContent here.";
        assert_eq!(strip_frontmatter(content), "# Body\nContent here.");
    }

    #[test]
    fn test_strip_frontmatter_no_frontmatter() {
        let content = "# Just a body\nNo frontmatter.";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[test]
    fn test_strip_frontmatter_no_closing() {
        let content = "---\nname: test\ndescription: test\nNo closing delimiter.";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[tokio::test]
    async fn test_load_skill_with_references() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("with-refs");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();

        // Write SKILL.md
        let mut f = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        write!(
            f,
            "---\nname: with-refs\ndescription: Skill with references\n---\n# Main Skill"
        )
        .unwrap();

        // Write reference files
        std::fs::write(
            skill_dir.join("references/REFERENCE.md"),
            "# Reference\nDetailed docs.",
        )
        .unwrap();

        let configs = vec![SkillConfig {
            name: "with-refs".to_string(),
            description: "Skill with references".to_string(),
            path: skill_dir,
        }];

        let tool = LoadSkillTool::new(&configs);
        let result = tool
            .call(LoadSkillArgs {
                name: "with-refs".to_string(),
            })
            .await
            .unwrap();

        assert!(result.starts_with("# Main Skill"));
        assert!(result.contains("# Reference\nDetailed docs."));
    }
}
