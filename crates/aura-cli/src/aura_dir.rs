//! Discovery helpers for the user's `.aura/` directories.
//!
//! Two distinct concepts:
//!
//! - **Global** (`~/.aura/`): user-wide files like `cli.toml`, welcome
//!   templates, conversation history. Shared across all projects.
//! - **Project** (`<ancestor>/.aura/`): the closest `.aura/` directory found
//!   by walking up from the current working directory. Holds project-local
//!   overrides (`cli.toml`) and machine-mutated state (`permissions.json`).
//!   By design there is no global permissions file — the user can always
//!   answer "what permissions am I running with?" by looking at one place
//!   relative to where they invoked the CLI.
//!
//! The walk explicitly skips `$HOME` so a user's home `.aura/` is never
//! treated as a project root (otherwise running the CLI from `$HOME` or any
//! ancestor of it would silently apply permissions and overrides).

use std::path::{Path, PathBuf};

/// Walk up from `start` and return the first ancestor that contains a
/// `.aura/` directory, skipping `$HOME` itself. Returns `None` if no such
/// ancestor exists.
///
/// Closest-wins semantics — same convention as `.git`, `.editorconfig`,
/// `package.json`. Nested projects with their own `.aura/` shadow ancestor
/// `.aura/` directories naturally.
pub fn find_project_aura_dir(start: &Path) -> Option<PathBuf> {
    find_project_aura_dir_with_home(start, dirs::home_dir().as_deref())
}

/// Same as [`find_project_aura_dir`] but with an injectable home directory.
/// Exposed so tests can avoid depending on the developer's real `$HOME`.
pub fn find_project_aura_dir_with_home(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        if home == Some(ancestor) {
            continue;
        }
        let candidate = ancestor.join(".aura");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Return `~/.aura/`, or `None` if the home directory cannot be determined.
pub fn global_aura_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".aura"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn finds_aura_dir_in_cwd() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".aura")).unwrap();

        let found = find_project_aura_dir(tmp.path()).unwrap();
        assert_eq!(found, tmp.path().join(".aura"));
    }

    #[test]
    fn walks_up_to_ancestor_aura_dir() {
        let tmp = TempDir::new().unwrap();
        let project_aura = tmp.path().join(".aura");
        fs::create_dir(&project_aura).unwrap();

        let deep = tmp.path().join("deeper").join("even-deeper").join("HERE");
        fs::create_dir_all(&deep).unwrap();

        let found = find_project_aura_dir(&deep).unwrap();
        assert_eq!(found, project_aura);
    }

    #[test]
    fn closest_aura_dir_wins_over_ancestor() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".aura")).unwrap();

        let nested = tmp.path().join("nested-project");
        fs::create_dir(&nested).unwrap();
        let nested_aura = nested.join(".aura");
        fs::create_dir(&nested_aura).unwrap();

        let inner = nested.join("src");
        fs::create_dir(&inner).unwrap();

        let found = find_project_aura_dir(&inner).unwrap();
        assert_eq!(found, nested_aura);
    }

    #[test]
    fn returns_none_when_no_aura_dir_anywhere() {
        let tmp = TempDir::new().unwrap();
        let inner = tmp.path().join("a").join("b");
        fs::create_dir_all(&inner).unwrap();

        // Walk will reach filesystem root without finding `.aura/`.
        // (We're outside any path that has one, modulo whatever the test
        // environment has at /tmp's ancestors — none on a typical CI box.)
        let found = find_project_aura_dir(&inner);
        assert!(
            found.is_none() || !found.unwrap().starts_with(tmp.path()),
            "expected no project .aura/ inside the temp tree"
        );
    }
}
