//! Shared test fixtures for `aura_events` overview types.

use aura_events::{AgentInfo, WorkerOverview};

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
        model: "gpt-4o".to_string(),
        workers,
    }
}
