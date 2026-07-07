use std::borrow::Cow;

use aura_events::{AgentInfo, WorkerOverview};

use crate::theme::{AuraStyle, Themed};
use crate::ui::state::term_size;
use crate::ui::text::wrap_words;

const INDENT: &str = "  ";

/// Print the agent overview block followed by a blank line.
pub fn print_agent_overview(agent: &AgentInfo) {
    for line in agent_overview_block_lines(agent) {
        println!("{line}");
    }
    println!();
}

fn agent_overview_block_lines(agent: &AgentInfo) -> Vec<String> {
    let mut lines = Vec::with_capacity(agent.workers.len() + 3);
    let role = if agent.workers.is_empty() {
        "model"
    } else {
        "coordinator"
    };

    lines.push(format!("{}", "Agent".themed(AuraStyle::Heading)));
    lines.push(format!(
        "{INDENT}{} {} {} {}: {}",
        "•".themed(AuraStyle::Muted),
        agent.id.as_str().themed(AuraStyle::Heading),
        "—".themed(AuraStyle::Muted),
        role.themed(AuraStyle::Muted),
        agent.model.as_str().themed(AuraStyle::Muted),
    ));

    lines.extend(worker_block_lines(&agent.workers));

    lines
}

fn worker_block_lines(workers: &[WorkerOverview]) -> Vec<String> {
    if workers.is_empty() {
        return Vec::new();
    }

    let (w, _) = term_size();
    let width = w as usize;
    let mut lines = Vec::with_capacity(workers.len() + 1);
    lines.push(format!("{}", "Workers".themed(AuraStyle::Heading)));

    for worker in workers {
        let text = match &worker.model {
            Some(model) => Cow::Owned(format!("{} ({model})", worker.description)),
            None => Cow::Borrowed(worker.description.as_str()),
        };
        let prefix_len = INDENT.len() + 2 + worker.name.chars().count() + 3;
        let wrapped = wrap_words(&text, width.saturating_sub(prefix_len));

        lines.push(format!(
            "{INDENT}{} {} {} {}",
            "•".themed(AuraStyle::Muted),
            worker.name.as_str().themed(AuraStyle::Heading),
            "—".themed(AuraStyle::Muted),
            wrapped[0].as_str().themed(AuraStyle::Muted),
        ));
        let hang = " ".repeat(prefix_len);
        for line in &wrapped[1..] {
            lines.push(format!("{hang}{}", line.as_str().themed(AuraStyle::Muted)));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{agent, worker};

    #[test]
    fn agent_overview_renders_single_agent_model_without_workers() {
        let agent = agent("ui-orch", Vec::new());
        let all = agent_overview_block_lines(&agent).join("\n");

        assert!(all.contains("Agent"));
        assert!(all.contains("ui-orch"));
        assert!(all.contains("model"));
        assert!(all.contains("gpt-4o"));
        assert!(!all.contains("Workers"));
    }

    #[test]
    fn agent_overview_renders_coordinator_and_workers() {
        let agent = agent("ui-orch", vec![worker("planner"), worker("writer")]);
        let all = agent_overview_block_lines(&agent).join("\n");

        assert!(all.contains("Agent"));
        assert!(all.contains("coordinator"));
        assert!(all.contains("gpt-4o"));
        assert!(all.contains("Workers"));
        assert!(all.contains("planner"));
        assert!(all.contains("writer"));
    }

    #[test]
    fn worker_block_annotates_model_overrides() {
        let mut planner = worker("planner");
        planner.model = Some("gpt-4o-mini".to_string());
        let all = worker_block_lines(&[planner, worker("writer")]).join("\n");
        assert!(all.contains("(gpt-4o-mini)"));
        assert_eq!(all.matches('(').count(), 1);
    }
}
