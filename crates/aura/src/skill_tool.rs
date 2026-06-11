//! On-demand skill loading tools.
//!
//! Provides the progressive-disclosure tool pair from the Agent Skills
//! specification (<https://agentskills.io/specification>): the skill catalog
//! lives in the system prompt, `load_skill` returns a skill's SKILL.md body
//! plus a listing of its resource files, and `read_skill_file` retrieves an
//! individual resource only when the LLM asks for it.
//!
//! Skills are discovered from directories containing a `SKILL.md` file with
//! YAML frontmatter. Content is read from disk on demand, keeping the base
//! system prompt small.

use crate::config::SkillConfig;
use rig::{completion::ToolDefinition, tool::Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

/// Size threshold (in bytes) above which a warning is logged.
/// Skills are still loaded in full — the operator controls their own content
/// and the LLM's context window is the natural limit.
const SKILL_SIZE_WARN_THRESHOLD: usize = 524_288; // 512 KB

/// Resource subdirectories defined by the Agent Skills specification.
const RESOURCE_DIRS: [&str; 3] = ["references", "scripts", "assets"];

/// Tool that loads skill content on demand from disk.
///
/// The LLM sees a catalog of available skills in the tool description
/// and calls `load_skill("skill_name")` when it needs detailed instructions.
#[derive(Debug, Clone)]
pub struct LoadSkillTool {
    skills: Arc<[SkillConfig]>,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("Unknown skill: '{0}'. Use one of the available skill names.")]
    UnknownSkill(String),

    #[error("Failed to read skill file: {0}")]
    IoError(#[from] std::io::Error),

    #[error(
        "Resource '{path}' not found in skill '{skill}'. Use a path from the skill's resource listing."
    )]
    ResourceNotFound { skill: String, path: String },

    #[error(
        "Path '{requested}' escapes the skill directory. Use a relative path from the skill's resource listing."
    )]
    PathEscape { requested: String },
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

/// List resource files available under a skill directory.
///
/// Scans the spec-defined `references/`, `scripts/`, and `assets/`
/// subdirectories one level deep and returns sorted relative paths like
/// `references/REFERENCE.md`. Missing subdirectories are skipped.
async fn list_skill_resources(skill_dir: &Path) -> Vec<String> {
    let Ok(canonical_dir) = skill_dir.canonicalize() else {
        return Vec::new();
    };
    let mut resources = Vec::new();
    for subdir in RESOURCE_DIRS {
        let Ok(mut entries) = fs::read_dir(skill_dir.join(subdir)).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            // Only advertise paths read_skill_file will accept: resolve
            // symlinks and require containment, so the listing never names a
            // path the traversal guard then refuses.
            let listable = entry
                .path()
                .canonicalize()
                .is_ok_and(|p| p.is_file() && p.starts_with(&canonical_dir));
            if listable {
                resources.push(format!("{subdir}/{}", entry.file_name().to_string_lossy()));
            }
        }
    }
    resources.sort();
    resources
}

/// Render the system-prompt skill catalog, or `None` when no skills are
/// configured.
pub fn render_skill_catalog(skills: &[SkillConfig]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut catalog = String::from(
        "\n\nAvailable skills (use the `load_skill` tool to load before answering):\n",
    );
    for skill in skills {
        catalog.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    Some(catalog)
}

/// A path proven to resolve inside a skill directory.
pub struct SkillResourcePath(PathBuf);

impl SkillResourcePath {
    /// Resolve a requested relative path against a skill directory,
    /// rejecting anything that escapes it.
    pub fn resolve(skill_dir: &Path, requested: &str) -> Result<Self, SkillError> {
        let requested_path = Path::new(requested);
        let has_escape_component = requested_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        });
        if has_escape_component {
            return Err(SkillError::PathEscape {
                requested: requested.to_owned(),
            });
        }

        let skill_name = skill_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let canonical_dir = skill_dir.canonicalize()?;
        let joined = skill_dir.join(requested_path);

        // The lexical check above cannot see through symlinks; canonicalize
        // resolves them, so the prefix check is what defeats a symlink
        // pointing outside the skill directory.
        match joined.canonicalize() {
            Ok(canonical) if canonical.starts_with(&canonical_dir) => Ok(Self(canonical)),
            Ok(_) => Err(SkillError::PathEscape {
                requested: requested.to_owned(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Classify the miss only after a containment check on the
                // deepest existing ancestor — otherwise NotFound-vs-PathEscape
                // becomes an existence oracle for paths outside the skill
                // directory whenever the skill ships an outward symlink.
                let escapes = joined
                    .ancestors()
                    .find_map(|ancestor| ancestor.canonicalize().ok())
                    .is_some_and(|ancestor| !ancestor.starts_with(&canonical_dir));
                if escapes {
                    Err(SkillError::PathEscape {
                        requested: requested.to_owned(),
                    })
                } else {
                    Err(SkillError::ResourceNotFound {
                        skill: skill_name,
                        path: requested.to_owned(),
                    })
                }
            }
            Err(e) => Err(SkillError::IoError(e)),
        }
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Tool that reads an individual resource file from a named skill.
///
/// The LLM learns available paths from the resource listing appended to
/// `load_skill` output and fetches only the files it needs.
#[derive(Debug, Clone)]
pub struct ReadSkillFileTool {
    skills: Arc<[SkillConfig]>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ReadSkillFileArgs {
    /// Name of the skill that owns the resource
    pub skill: String,
    /// Relative path to the resource file within the skill directory
    pub path: String,
}

impl Tool for ReadSkillFileTool {
    const NAME: &'static str = "read_skill_file";
    type Error = SkillError;
    type Args = ReadSkillFileArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read a resource file from a named skill. Use one of the relative \
                          paths listed under 'Skill resources' in the output of `load_skill`."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "Name of the skill that owns the resource"
                    },
                    "path": {
                        "type": "string",
                        "description": "Relative path to the resource file, e.g. 'references/REFERENCE.md'"
                    }
                },
                "required": ["skill", "path"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let skill = self
            .skills
            .iter()
            .find(|s| s.name == args.skill)
            .ok_or_else(|| SkillError::UnknownSkill(args.skill.clone()))?;

        let resource = SkillResourcePath::resolve(&skill.path, &args.path)?;
        tracing::info!(
            "Reading skill '{}' resource '{}' from {:?}",
            skill.name,
            args.path,
            resource.as_path()
        );

        let content = fs::read_to_string(resource.as_path()).await?;
        if content.len() > SKILL_SIZE_WARN_THRESHOLD {
            tracing::warn!(
                "Skill '{}' resource '{}' is large ({} bytes). This may consume significant \
                 LLM context window. Consider splitting into smaller files if possible.",
                skill.name,
                args.path,
                content.len()
            );
        }
        Ok(content)
    }
}

/// The skill tool pair sharing a single skill list.
pub struct SkillToolset {
    pub load: LoadSkillTool,
    pub read_file: ReadSkillFileTool,
}

impl SkillToolset {
    /// Build both skill tools, or `None` when no skills are configured.
    pub fn new(skills: &[SkillConfig]) -> Option<Self> {
        if skills.is_empty() {
            return None;
        }
        let skills: Arc<[SkillConfig]> = skills.into();
        Some(Self {
            load: LoadSkillTool {
                skills: Arc::clone(&skills),
            },
            read_file: ReadSkillFileTool { skills },
        })
    }
}

impl LoadSkillTool {
    /// Create a new LoadSkillTool from discovered skill configs.
    ///
    /// Each `SkillConfig` contains an absolute path to the skill directory.
    pub fn new(skills: &[SkillConfig]) -> Self {
        Self {
            skills: skills.into(),
        }
    }

    /// Build the tool description listing all available skills.
    fn build_description(&self) -> String {
        let mut desc =
            String::from("Load detailed instructions for a specific skill. Available skills:\n");
        for skill in self.skills.iter() {
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

        if body.len() > SKILL_SIZE_WARN_THRESHOLD {
            tracing::warn!(
                "Skill '{}' content is large ({} bytes). This may consume significant \
                 LLM context window. Consider splitting into smaller skills if possible.",
                skill.name,
                body.len()
            );
        }

        // Progressive disclosure: list resource files instead of inlining
        // them, so the LLM fetches only what it needs via read_skill_file.
        let mut result = body.to_string();
        let resources = list_skill_resources(&skill.path).await;
        if !resources.is_empty() {
            result.push_str(
                "\n\n## Skill resources\nLoad any of these with the `read_skill_file` tool when needed:\n",
            );
            for resource in &resources {
                result.push_str(&format!("- {resource}\n"));
            }
        }

        tracing::info!(
            "Skill '{}' loaded ({} bytes body, {} resources listed)",
            skill.name,
            body.len(),
            resources.len()
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

    fn make_skill_dir(dir: &std::path::Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
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

    /// Skill dir with one resource file in each spec subdirectory.
    fn make_skill_with_resources(dir: &std::path::Path) -> Vec<SkillConfig> {
        let skill_dir = make_skill_dir(
            dir,
            "with-refs",
            "---\nname: with-refs\ndescription: Skill with references\n---\n",
            "# Main Skill",
        );
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("references/REFERENCE.md"),
            "# Reference\nDetailed docs.",
        )
        .unwrap();

        vec![SkillConfig {
            name: "with-refs".to_string(),
            description: "Skill with references".to_string(),
            path: skill_dir,
        }]
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
        let configs = make_skill_with_resources(dir.path());

        let tool = LoadSkillTool::new(&configs);
        let result = tool
            .call(LoadSkillArgs {
                name: "with-refs".to_string(),
            })
            .await
            .unwrap();

        assert!(result.starts_with("# Main Skill"));
        assert!(result.contains("## Skill resources"));
        assert!(result.contains("- references/REFERENCE.md"));
        // Progressive disclosure: the resource is listed, not inlined.
        assert!(!result.contains("Detailed docs."));
    }

    #[tokio::test]
    async fn test_load_skill_without_resources_omits_section() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_configs(dir.path());
        let tool = LoadSkillTool::new(&configs);
        let result = tool
            .call(LoadSkillArgs {
                name: "test-skill".to_string(),
            })
            .await
            .unwrap();
        assert!(!result.contains("## Skill resources"));
    }

    #[tokio::test]
    async fn test_list_skill_resources_sorted_one_level_deep() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("resourceful");
        std::fs::create_dir_all(skill_dir.join("references/nested")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(skill_dir.join("assets")).unwrap();
        std::fs::write(skill_dir.join("references/b.md"), "b").unwrap();
        std::fs::write(skill_dir.join("references/a.md"), "a").unwrap();
        std::fs::write(skill_dir.join("references/nested/deep.md"), "deep").unwrap();
        std::fs::write(skill_dir.join("scripts/run.sh"), "echo hi").unwrap();
        std::fs::write(skill_dir.join("assets/logo.svg"), "<svg/>").unwrap();

        let resources = list_skill_resources(&skill_dir).await;
        assert_eq!(
            resources,
            vec![
                "assets/logo.svg",
                "references/a.md",
                "references/b.md",
                "scripts/run.sh",
            ]
        );
    }

    #[tokio::test]
    async fn test_read_skill_file_success() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_with_resources(dir.path());
        let toolset = SkillToolset::new(&configs).unwrap();
        let result = toolset
            .read_file
            .call(ReadSkillFileArgs {
                skill: "with-refs".to_string(),
                path: "references/REFERENCE.md".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(result, "# Reference\nDetailed docs.");
    }

    #[tokio::test]
    async fn test_read_skill_file_parent_dir_escape() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("escape.md"), "secret").unwrap();
        let configs = make_skill_with_resources(dir.path());
        let toolset = SkillToolset::new(&configs).unwrap();
        let result = toolset
            .read_file
            .call(ReadSkillFileArgs {
                skill: "with-refs".to_string(),
                path: "../escape.md".to_string(),
            })
            .await;
        assert!(matches!(result, Err(SkillError::PathEscape { .. })));
    }

    #[tokio::test]
    async fn test_read_skill_file_absolute_path_escape() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_with_resources(dir.path());
        let toolset = SkillToolset::new(&configs).unwrap();
        let result = toolset
            .read_file
            .call(ReadSkillFileArgs {
                skill: "with-refs".to_string(),
                path: "/etc/hosts".to_string(),
            })
            .await;
        assert!(matches!(result, Err(SkillError::PathEscape { .. })));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_skill_file_symlink_escape() {
        let dir = TempDir::new().unwrap();
        let outside = dir.path().join("outside.md");
        std::fs::write(&outside, "outside content").unwrap();
        let configs = make_skill_with_resources(dir.path());
        std::os::unix::fs::symlink(&outside, configs[0].path.join("references/link.md")).unwrap();

        let toolset = SkillToolset::new(&configs).unwrap();
        let result = toolset
            .read_file
            .call(ReadSkillFileArgs {
                skill: "with-refs".to_string(),
                path: "references/link.md".to_string(),
            })
            .await;
        assert!(matches!(result, Err(SkillError::PathEscape { .. })));
    }

    /// An outward symlink must not turn NotFound-vs-PathEscape into an
    /// existence oracle for paths outside the skill directory: a miss under
    /// an escaping ancestor reports PathEscape regardless of leaf existence.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_skill_file_symlink_escape_missing_leaf_is_path_escape() {
        let dir = TempDir::new().unwrap();
        let outside_dir = dir.path().join("outside");
        std::fs::create_dir(&outside_dir).unwrap();
        let configs = make_skill_with_resources(dir.path());
        std::os::unix::fs::symlink(&outside_dir, configs[0].path.join("references/extdir"))
            .unwrap();

        let toolset = SkillToolset::new(&configs).unwrap();
        let result = toolset
            .read_file
            .call(ReadSkillFileArgs {
                skill: "with-refs".to_string(),
                path: "references/extdir/missing.md".to_string(),
            })
            .await;
        assert!(matches!(result, Err(SkillError::PathEscape { .. })));
    }

    /// The resource listing must never advertise a path the traversal guard
    /// refuses: outward symlinks are excluded from load_skill's listing.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_listing_excludes_escaping_symlink() {
        let dir = TempDir::new().unwrap();
        let outside = dir.path().join("outside.md");
        std::fs::write(&outside, "outside content").unwrap();
        let configs = make_skill_with_resources(dir.path());
        std::os::unix::fs::symlink(&outside, configs[0].path.join("references/link.md")).unwrap();

        let listing = list_skill_resources(&configs[0].path).await;
        assert!(!listing.iter().any(|p| p.contains("link.md")));
        assert!(listing.iter().any(|p| p.contains("references/")));
    }

    #[tokio::test]
    async fn test_read_skill_file_not_found() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_with_resources(dir.path());
        let toolset = SkillToolset::new(&configs).unwrap();
        let result = toolset
            .read_file
            .call(ReadSkillFileArgs {
                skill: "with-refs".to_string(),
                path: "references/missing.md".to_string(),
            })
            .await;
        assert!(matches!(result, Err(SkillError::ResourceNotFound { .. })));
    }

    #[tokio::test]
    async fn test_read_skill_file_unknown_skill() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_with_resources(dir.path());
        let toolset = SkillToolset::new(&configs).unwrap();
        let result = toolset
            .read_file
            .call(ReadSkillFileArgs {
                skill: "nonexistent".to_string(),
                path: "references/REFERENCE.md".to_string(),
            })
            .await;
        assert!(matches!(result, Err(SkillError::UnknownSkill(_))));
    }

    #[test]
    fn test_render_skill_catalog_empty() {
        assert!(render_skill_catalog(&[]).is_none());
    }

    #[test]
    fn test_render_skill_catalog_text() {
        let dir = TempDir::new().unwrap();
        let configs = make_skill_configs(dir.path());
        let catalog = render_skill_catalog(&configs).unwrap();
        assert_eq!(
            catalog,
            "\n\nAvailable skills (use the `load_skill` tool to load before answering):\n\
             - test-skill: A test skill\n\
             - another-skill: Another test skill\n"
        );
    }

    #[test]
    fn test_skill_toolset_empty() {
        assert!(SkillToolset::new(&[]).is_none());
    }
}
