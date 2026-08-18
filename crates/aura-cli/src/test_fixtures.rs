//! Shared test fixtures: `aura_events` overview types and terminal-output
//! helpers.

use aura_events::{AgentInfo, WorkerOverview};

/// Strip SGR sequences (`ESC[…m`) so layout assertions are theme-independent.
pub(crate) fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// [`strip_sgr`] over every line.
pub(crate) fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| strip_sgr(l)).collect()
}

pub(crate) fn worker(name: &str) -> WorkerOverview {
    WorkerOverview {
        name: name.to_string(),
        description: format!("{name} does work"),
        model: None,
    }
}

pub(crate) fn agent(id: &str, workers: Vec<WorkerOverview>) -> AgentInfo {
    AgentInfo {
        id: id.to_string(),
        description: None,
        model: "gpt-4o".to_string(),
        workers,
        mcp_servers: None,
    }
}
