//! Projections of an agent [`Config`] into the wire types served by
//! `GET /aura/info` ([`AgentInfo`], [`WorkerOverview`]).

use aura_config::Config;
use aura_events::{AgentInfo, WorkerOverview};

/// Project a config into its `/aura/info` agent entry: identifier, LLM model
/// name, and worker overview.
pub fn agent_info(config: &Config) -> AgentInfo {
    AgentInfo {
        id: config.agent_id().to_owned(),
        model: config.agent.llm.model_info().1.to_owned(),
        workers: worker_overview(config),
    }
}

/// Summarize a config's orchestration workers, sorted by name. Empty when
/// orchestration is disabled. A worker's `model` is set only when its LLM
/// override resolves to a different model than the coordinator's.
pub fn worker_overview(config: &Config) -> Vec<WorkerOverview> {
    let Some(orch) = config.orchestration.as_ref().filter(|o| o.enabled) else {
        return Vec::new();
    };

    let coordinator_model = config.agent.llm.model_info().1;
    let mut workers: Vec<_> = orch
        .workers
        .iter()
        .map(|(name, worker)| {
            let worker_model = worker
                .llm
                .as_ref()
                .unwrap_or(&config.agent.llm)
                .model_info()
                .1;
            WorkerOverview {
                name: name.clone(),
                description: worker.description.clone(),
                model: (worker_model != coordinator_model).then(|| worker_model.to_owned()),
            }
        })
        .collect();
    workers.sort_by(|a, b| a.name.cmp(&b.name));
    workers
}

#[cfg(test)]
mod tests {
    use super::worker_overview;
    use aura_config::load_config_from_str;

    #[test]
    fn test_worker_overview_empty_when_orchestration_disabled() {
        let config = load_config_from_str(
            r#"
[agent]
name = "solo"
system_prompt = "You are solo."
[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"

[orchestration]
enabled = false

[orchestration.worker.x]
description = "Defined but disabled"
preamble = "p"
"#,
        )
        .expect("config should parse");

        assert!(worker_overview(&config).is_empty());
    }

    #[test]
    fn test_worker_overview_sorts_and_annotates_only_overridden_models() {
        let config = load_config_from_str(
            r#"
[agent]
name = "orch"
system_prompt = "You are orch."
[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"

[orchestration]
enabled = true

[orchestration.worker.beta]
description = "Runs a different model"
preamble = "p"
[orchestration.worker.beta.llm]
provider = "openai"
model = "gpt-4o-mini"
api_key = "k"

[orchestration.worker.alpha]
description = "Inherits coordinator model"
preamble = "p"

[orchestration.worker.charlie]
description = "Overrides to the same model"
preamble = "p"
[orchestration.worker.charlie.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"
"#,
        )
        .expect("config should parse");

        let workers = worker_overview(&config);
        let names: Vec<_> = workers.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "charlie"]);
        assert_eq!(workers[0].model, None);
        assert_eq!(workers[1].model, Some("gpt-4o-mini".to_string()));
        assert_eq!(workers[2].model, None);
    }
}
