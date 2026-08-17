//! Discovery of the TOML **agent** config that standalone mode runs.
//!
//! Distinct from [`crate::config`], which resolves the CLI's own preferences
//! (`cli.toml`). This module answers "which agent(s) should I build?".
//!
//! Search order, first hit wins:
//!
//! 1. `--config <path>` / `AURA_CONFIG`
//! 2. `config.toml` in the current working directory
//! 3. `~/.aura/agents/` — a directory of agent configs
//! 4. `~/.aura/agent.toml`
//!
//! The current-directory lookup deliberately does not walk up through parent
//! directories: a stray `config.toml` several levels up would take effect
//! without the user noticing which file they were running.
//!
//! [`aura_config::load_config`] accepts a file or a directory, so the
//! `~/.aura/agents/` candidate loads every `*.toml` in it at once and `/model`
//! switches between them.
//!
//! `~/.aura/config.toml` is **not** a candidate — that name belongs to the
//! pre-rename CLI preferences file that [`crate::config`] still reads.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::aura_dir::global_aura_dir;

/// Agent-config filename in the current working directory.
const CWD_CONFIG_FILENAME: &str = "config.toml";

/// Directory of agent configs under `~/.aura/`.
const GLOBAL_AGENTS_DIRNAME: &str = "agents";

/// Single-file agent config under `~/.aura/`.
const GLOBAL_AGENT_FILENAME: &str = "agent.toml";

/// Return `~/.aura/agents/`, or `None` if the home directory cannot be
/// determined.
pub fn global_agents_dir() -> Option<PathBuf> {
    global_aura_dir().map(|d| d.join(GLOBAL_AGENTS_DIRNAME))
}

/// Resolve the agent config path for standalone mode.
pub fn resolve(explicit: Option<&str>) -> Result<PathBuf> {
    resolve_in(
        explicit,
        &std::env::current_dir()?,
        global_aura_dir().as_deref(),
    )
}

/// Same as [`resolve`] but with the working directory and `~/.aura/`
/// injected, so tests need neither a real `$HOME` nor a process-wide `chdir`.
///
/// An explicit path is returned verbatim and unchecked: a typo'd `--config`
/// must report the path the user actually typed rather than silently falling
/// through to a global config.
pub fn resolve_in(
    explicit: Option<&str>,
    cwd: &Path,
    global_aura_dir: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(PathBuf::from(path));
    }

    let mut candidates = candidates(cwd, global_aura_dir);
    match candidates.iter().position(|c| is_usable(c)) {
        Some(found) => Ok(candidates.remove(found)),
        None => Err(anyhow::anyhow!(missing_config_message(&candidates))),
    }
}

fn candidates(cwd: &Path, global_aura_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = vec![cwd.join(CWD_CONFIG_FILENAME)];
    if let Some(global) = global_aura_dir {
        candidates.push(global.join(GLOBAL_AGENTS_DIRNAME));
        candidates.push(global.join(GLOBAL_AGENT_FILENAME));
    }
    candidates
}

fn is_usable(path: &Path) -> bool {
    if !path.is_dir() {
        return path.is_file();
    }
    // An empty directory falls through to the next candidate, so a leftover
    // `~/.aura/agents/` does not strand startup. One that cannot be read stays
    // a candidate instead, letting the loader surface the IO error rather than
    // the search claiming nothing exists anywhere.
    match std::fs::read_dir(path) {
        Ok(entries) => entries
            .filter_map(std::result::Result::ok)
            .any(|e| e.path().extension().is_some_and(|ext| ext == "toml")),
        Err(_) => true,
    }
}

/// Build the actionable "no agent config" startup error for the locations in
/// `searched`.
///
/// A single location reads as a direct "not at this path" — that is the
/// explicit `--config` case, where listing fallbacks the CLI never consulted
/// would be misleading. Several locations are listed so the user can see the
/// global fallbacks exist.
pub fn missing_config_message<P: AsRef<Path>>(searched: &[P]) -> String {
    let prog = program_name();
    let heading = match searched {
        [only] => format!("No agent config found at `{}`.", only.as_ref().display()),
        _ => {
            let list = searched
                .iter()
                .map(|p| format!("  • {}", p.as_ref().display()))
                .collect::<Vec<_>>()
                .join("\n");
            format!("No agent config found. Searched:\n{list}")
        }
    };
    format!(
        "{heading}\n\n\
         Standalone mode needs a TOML agent config. To get started:\n  \
         • run `{prog} init` to generate one (it can install to `~/.aura/agents/` \
         so every directory picks it up)\n  \
         • pass `--config <path>` (or set AURA_CONFIG) to point at an existing \
         config file or directory\n  \
         • set `--api-url <url>` (or AURA_API_URL) to connect to a running \
         aura-web-server instead"
    )
}

/// Derive the program name from the running executable rather than
/// hardcoding it, so suggested commands stay correct if the binary is
/// renamed (e.g. `aura-cli` -> `aura`).
fn program_name() -> String {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "aura".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "").unwrap();
    }

    #[test]
    fn explicit_path_wins_and_is_not_checked() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("config.toml"));

        let resolved = resolve_in(Some("/nope/custom.toml"), tmp.path(), None).unwrap();
        assert_eq!(resolved, PathBuf::from("/nope/custom.toml"));
    }

    #[test]
    fn cwd_config_wins_over_global() {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        touch(&cwd.path().join("config.toml"));
        touch(&home.path().join("agents").join("assistant.toml"));

        let resolved = resolve_in(None, cwd.path(), Some(home.path())).unwrap();
        assert_eq!(resolved, cwd.path().join("config.toml"));
    }

    #[test]
    fn falls_back_to_global_agents_dir() {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        touch(&home.path().join("agents").join("assistant.toml"));

        let resolved = resolve_in(None, cwd.path(), Some(home.path())).unwrap();
        assert_eq!(resolved, home.path().join("agents"));
    }

    #[test]
    fn empty_global_agents_dir_falls_through_to_agent_toml() {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        fs::create_dir_all(home.path().join("agents")).unwrap();
        touch(&home.path().join("agent.toml"));

        let resolved = resolve_in(None, cwd.path(), Some(home.path())).unwrap();
        assert_eq!(resolved, home.path().join("agent.toml"));
    }

    #[test]
    fn non_toml_files_do_not_make_the_agents_dir_usable() {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        touch(&home.path().join("agents").join("README.md"));
        touch(&home.path().join("agent.toml"));

        let resolved = resolve_in(None, cwd.path(), Some(home.path())).unwrap();
        assert_eq!(resolved, home.path().join("agent.toml"));
    }

    #[test]
    fn legacy_global_config_toml_is_not_a_candidate() {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        // The pre-rename CLI preferences file — must never be loaded as an
        // agent config.
        touch(&home.path().join("config.toml"));

        let err = resolve_in(None, cwd.path(), Some(home.path())).unwrap_err();
        assert!(
            !err.to_string().contains(
                home.path()
                    .join("config.toml")
                    .display()
                    .to_string()
                    .as_str()
            ),
            "got: {err}"
        );
    }

    #[test]
    fn error_lists_every_searched_location() {
        let cwd = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();

        let err = resolve_in(None, cwd.path(), Some(home.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&cwd.path().join("config.toml").display().to_string()));
        assert!(err.contains(&home.path().join("agents").display().to_string()));
        assert!(err.contains(&home.path().join("agent.toml").display().to_string()));
        assert!(err.contains("AURA_CONFIG"), "got: {err}");
    }

    #[test]
    fn single_searched_location_reads_as_a_direct_miss() {
        let searched = vec![PathBuf::from("custom.toml")];
        let msg = missing_config_message(&searched);
        assert!(msg.starts_with("No agent config found at `custom.toml`."));
        assert!(!msg.contains("Searched:"));
    }
}
