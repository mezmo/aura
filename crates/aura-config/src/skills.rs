//! Skill discovery from local source directories.
//!
//! Skills follow the Agent Skills specification
//! (<https://agentskills.io/specification>): each skill is a directory
//! containing a `SKILL.md` file with YAML frontmatter. Discovery validates
//! names against the spec and resolves relative sources against the process
//! current working directory.

use crate::ConfigError;
use crate::config::{LocalSkillSource, SkillConfig};
use std::collections::HashMap;
use std::path::PathBuf;

/// YAML frontmatter parsed from SKILL.md files.
///
/// Follows the Agent Skills specification (<https://agentskills.io/specification>).
/// Optional spec fields (`license`, `compatibility`, `metadata`, `allowed-tools`)
/// are intentionally ignored — the catalog only needs `name` and `description`.
#[derive(Debug, serde::Deserialize)]
struct SkillFrontmatter {
    /// Required: must match the parent directory name, 1-64 chars,
    /// lowercase alphanumeric and hyphens only.
    name: String,
    /// Required: 1-1024 chars describing what the skill does and when to use it.
    description: String,
}

/// A skill identifier, validated against the Agent Skills specification.
///
/// The only ways to obtain one are [`SkillName::new`] and deserialization,
/// both of which run the spec validation, so holding a `SkillName` proves the
/// rules held: 1-64 characters, lowercase alphanumerics and hyphens, no
/// leading/trailing/consecutive hyphens.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
pub struct SkillName(String);

impl SkillName {
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        validate_skill_name(&name)?;
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillName {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl std::fmt::Display for SkillName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SkillName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for SkillName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SkillName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for SkillName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

/// Validate a skill name per the Agent Skills specification.
fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!(
            "Skill name must be 1-64 characters, got {} characters",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "Skill name '{}' contains invalid characters (only lowercase alphanumeric and hyphens allowed)",
            name
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(format!(
            "Skill name '{}' must not start or end with a hyphen",
            name
        ));
    }
    if name.contains("--") {
        return Err(format!(
            "Skill name '{}' must not contain consecutive hyphens",
            name
        ));
    }
    Ok(())
}

/// Parse YAML frontmatter delimited by `---` from a SKILL.md file.
///
/// Also enforces the spec constraints: a valid skill name and a non-empty
/// description of at most 1024 characters.
fn parse_skill_frontmatter(content: &str) -> Result<SkillFrontmatter, ConfigError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(ConfigError::Validation(
            "SKILL.md must start with YAML frontmatter (---)".to_string(),
        ));
    }

    let after_first = &trimmed[3..];
    let closing = after_first.find("---").ok_or_else(|| {
        ConfigError::Validation("SKILL.md frontmatter missing closing ---".to_string())
    })?;

    let yaml_str = &after_first[..closing];

    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml_str).map_err(|e| {
        ConfigError::Validation(format!("Failed to parse SKILL.md frontmatter: {e}"))
    })?;

    if frontmatter.description.is_empty() {
        return Err(ConfigError::Validation(
            "SKILL.md frontmatter must include a non-empty 'description' field".to_string(),
        ));
    }

    if frontmatter.description.len() > 1024 {
        return Err(ConfigError::Validation(format!(
            "Skill description exceeds 1024 character limit (got {} characters)",
            frontmatter.description.len()
        )));
    }

    validate_skill_name(&frontmatter.name).map_err(ConfigError::Validation)?;

    Ok(frontmatter)
}

/// Discover skills from local source directories.
///
/// Scans each source directory for subdirectories containing a `SKILL.md` file.
/// The `name` field in frontmatter must match the directory name (per the spec).
pub fn discover_skills(sources: &[LocalSkillSource]) -> Result<Vec<SkillConfig>, ConfigError> {
    let mut skills = Vec::new();

    for source in sources {
        let source_path = &source.source;
        let resolved = source_path.canonicalize().map_err(|e| {
            ConfigError::Validation(format!(
                "Skill source directory '{}' not found: {e}",
                source_path.display()
            ))
        })?;

        tracing::info!("Discovering skills from: {}", resolved.display());

        let entries = std::fs::read_dir(&resolved).map_err(|e| {
            ConfigError::Validation(format!(
                "Cannot read skill source directory '{}': {e}",
                resolved.display()
            ))
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                ConfigError::Validation(format!("Error reading skill directory entry: {e}"))
            })?;

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let skill_file = path.join("SKILL.md");
            if !skill_file.exists() {
                tracing::debug!("Skipping directory '{}' (no SKILL.md)", path.display());
                continue;
            }

            let dir_name = entry.file_name().to_string_lossy().to_string();

            let content = std::fs::read_to_string(&skill_file).map_err(|e| {
                ConfigError::Validation(format!(
                    "Failed to read SKILL.md in '{}': {e}",
                    path.display()
                ))
            })?;

            let frontmatter = parse_skill_frontmatter(&content)?;

            // Per spec: name must match the parent directory name
            if frontmatter.name != dir_name {
                return Err(ConfigError::Validation(format!(
                    "Skill name '{}' in SKILL.md does not match directory name '{}'",
                    frontmatter.name, dir_name
                )));
            }

            tracing::info!(
                "  Discovered skill '{}': {}",
                dir_name,
                frontmatter.description
            );

            skills.push(SkillConfig {
                name: SkillName::new(dir_name).map_err(ConfigError::Validation)?,
                description: frontmatter.description,
                path: path.clone(),
            });
        }
    }

    // Deduplicate: keep the first occurrence, warn on duplicates
    let mut seen: HashMap<SkillName, PathBuf> = HashMap::new();
    skills.retain(|skill| {
        if let Some(existing_path) = seen.get(&skill.name) {
            tracing::warn!(
                "Duplicate skill '{}' found in '{}' (already loaded from '{}'), skipping",
                skill.name,
                skill.path.display(),
                existing_path.display()
            );
            false
        } else {
            seen.insert(skill.name.clone(), skill.path.clone());
            true
        }
    });

    // Sort by name for deterministic ordering
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(skills)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // validate_skill_name tests
    // -----------------------------------------------------------------------

    #[test]
    fn validate_skill_name_valid() {
        assert!(validate_skill_name("code-review").is_ok());
        assert!(validate_skill_name("a").is_ok());
        assert!(validate_skill_name("my-skill-123").is_ok());
    }

    #[test]
    fn validate_skill_name_empty() {
        let err = validate_skill_name("").unwrap_err();
        assert!(err.contains("1-64 characters"));
    }

    #[test]
    fn validate_skill_name_too_long() {
        let long_name = "a".repeat(65);
        let err = validate_skill_name(&long_name).unwrap_err();
        assert!(err.contains("1-64 characters"));
    }

    #[test]
    fn validate_skill_name_max_length_ok() {
        let name = "a".repeat(64);
        assert!(validate_skill_name(&name).is_ok());
    }

    #[test]
    fn validate_skill_name_uppercase_rejected() {
        let err = validate_skill_name("Code-Review").unwrap_err();
        assert!(err.contains("invalid characters"));
    }

    #[test]
    fn validate_skill_name_leading_hyphen() {
        let err = validate_skill_name("-code").unwrap_err();
        assert!(err.contains("must not start or end with a hyphen"));
    }

    #[test]
    fn validate_skill_name_trailing_hyphen() {
        let err = validate_skill_name("code-").unwrap_err();
        assert!(err.contains("must not start or end with a hyphen"));
    }

    #[test]
    fn validate_skill_name_consecutive_hyphens() {
        let err = validate_skill_name("code--review").unwrap_err();
        assert!(err.contains("consecutive hyphens"));
    }

    #[test]
    fn validate_skill_name_underscore_rejected() {
        let err = validate_skill_name("code_review").unwrap_err();
        assert!(err.contains("invalid characters"));
    }

    // -----------------------------------------------------------------------
    // parse_skill_frontmatter tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frontmatter_valid() {
        let content = "---\nname: my-skill\ndescription: A test skill\n---\n# Body";
        let fm = parse_skill_frontmatter(content).unwrap();
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "A test skill");
    }

    #[test]
    fn parse_frontmatter_missing_opening() {
        let content = "name: my-skill\ndescription: A test skill\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("must start with YAML frontmatter"));
    }

    #[test]
    fn parse_frontmatter_missing_closing() {
        let content = "---\nname: my-skill\ndescription: A test skill\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("missing closing"));
    }

    #[test]
    fn parse_frontmatter_empty_description() {
        let content = "---\nname: my-skill\ndescription: \"\"\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("non-empty 'description'"));
    }

    #[test]
    fn parse_frontmatter_description_too_long() {
        let long_desc = "a".repeat(1025);
        let content = format!("---\nname: my-skill\ndescription: {long_desc}\n---\n# Body");
        let err = parse_skill_frontmatter(&content).unwrap_err();
        assert!(err.to_string().contains("1024 character limit"));
    }

    #[test]
    fn parse_frontmatter_invalid_name() {
        let content = "---\nname: Code-Review\ndescription: A skill\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn parse_frontmatter_missing_name_field() {
        let content = "---\ndescription: A skill\n---\n# Body";
        let err = parse_skill_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // SkillFrontmatter has no deny_unknown_fields; these tests pin that the
    // optional agentskills.io spec fields stay tolerated (parsed fine, ignored)
    // so a future refactor can't silently break discovery of spec-complete skills.

    #[test]
    fn parse_frontmatter_tolerates_all_optional_spec_fields() {
        let content = r#"---
name: my-skill
description: A test skill
license: Apache-2.0
compatibility: Requires git and network access
metadata:
  author: example-org
  version: "1.2.0"
allowed-tools: Bash(git:*) Read
---
# Body"#;
        let fm = parse_skill_frontmatter(content).unwrap();
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "A test skill");
    }

    #[test]
    fn parse_frontmatter_tolerates_allowed_tools_string() {
        let content = "---\nname: my-skill\ndescription: A skill\nallowed-tools: Bash(git:*) Read\n---\n# Body";
        let fm = parse_skill_frontmatter(content).unwrap();
        assert_eq!(fm.name, "my-skill");
        assert_eq!(fm.description, "A skill");
    }

    // -----------------------------------------------------------------------
    // discover_skills tests
    // -----------------------------------------------------------------------

    fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut f = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        write!(
            f,
            "---\nname: {name}\ndescription: {description}\n---\n# {name}\nContent."
        )
        .unwrap();
    }

    #[test]
    fn discover_skills_happy_path() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "alpha", "Alpha skill");
        write_skill(dir.path(), "beta", "Beta skill");

        let sources = vec![LocalSkillSource {
            source: dir.path().to_path_buf(),
        }];
        let skills = discover_skills(&sources).unwrap();

        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[1].name, "beta");
    }

    #[test]
    fn discover_skills_skips_non_skill_dirs() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "real-skill", "A real skill");
        std::fs::create_dir_all(dir.path().join("not-a-skill")).unwrap();
        std::fs::write(dir.path().join("readme.md"), "# README").unwrap();

        let sources = vec![LocalSkillSource {
            source: dir.path().to_path_buf(),
        }];
        let skills = discover_skills(&sources).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "real-skill");
    }

    #[test]
    fn discover_skills_tolerates_optional_spec_fields() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("full-spec");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: full-spec
description: A skill using every optional spec field
license: MIT
compatibility: Requires docker
metadata:
  author: example-org
  version: "0.3.1"
allowed-tools: Bash(git:*) Read
---
# full-spec
Content."#,
        )
        .unwrap();

        let sources = vec![LocalSkillSource {
            source: dir.path().to_path_buf(),
        }];
        let skills = discover_skills(&sources).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "full-spec");
        assert_eq!(
            skills[0].description,
            "A skill using every optional spec field"
        );
    }

    #[test]
    fn discover_skills_name_mismatch_errors() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("wrong-name");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut f = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        write!(
            f,
            "---\nname: correct-name\ndescription: Mismatched\n---\n# Content"
        )
        .unwrap();

        let sources = vec![LocalSkillSource {
            source: dir.path().to_path_buf(),
        }];
        let err = discover_skills(&sources).unwrap_err();
        assert!(err.to_string().contains("does not match directory name"));
    }

    #[test]
    fn discover_skills_deduplicates() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        write_skill(dir1.path(), "shared", "First copy");
        write_skill(dir2.path(), "shared", "Second copy");

        let sources = vec![
            LocalSkillSource {
                source: dir1.path().to_path_buf(),
            },
            LocalSkillSource {
                source: dir2.path().to_path_buf(),
            },
        ];
        let skills = discover_skills(&sources).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "First copy");
    }

    #[test]
    fn discover_skills_relative_path_from_cwd() {
        // Relative sources resolve from the process current working directory.
        // To avoid parallel-test CWD flakiness, serialize CWD mutation and
        // restore it in a guard.
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = CWD_LOCK.lock().unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        let base = TempDir::new().unwrap();
        let skills_dir = base.path().join("my-skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        write_skill(&skills_dir, "test-skill", "A test");

        std::env::set_current_dir(base.path()).unwrap();
        let _guard = CwdGuard(original_cwd);

        let sources = vec![LocalSkillSource {
            source: "my-skills".into(),
        }];
        let skills = discover_skills(&sources).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
    }

    /// Restores the original working directory when dropped.
    struct CwdGuard(std::path::PathBuf);

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[test]
    fn discover_skills_nonexistent_source_errors() {
        let sources = vec![LocalSkillSource {
            source: "/nonexistent/path/to/skills".into(),
        }];
        let err = discover_skills(&sources).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
