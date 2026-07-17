use std::borrow::Cow;

use aura_events::{AgentInfo, WorkerOverview};

use crate::theme::{AuraStyle, Themed};
use crate::ui::state::term_size;
use crate::ui::text::wrap_words;

const INDENT: &str = "  ";

/// Minimum room beside the prefix for an inline description.
const MIN_DESC_WIDTH: usize = 20;

pub fn print_agent_overview(agent: &AgentInfo) {
    let (w, _) = term_size();
    for line in agent_overview_block_lines(agent, w as usize) {
        println!("{line}");
    }
    println!();
}

/// Separate from [`print_agent_overview`] so the CTA never re-appears when
/// `/model` re-displays the overview.
pub fn print_startup_cta(agent: &AgentInfo) {
    let (w, _) = term_size();
    for line in startup_cta_lines(agent, w as usize) {
        println!("{line}");
    }
    println!();
}

fn agent_overview_block_lines(agent: &AgentInfo, width: usize) -> Vec<String> {
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

    lines.extend(worker_block_lines(&agent.workers, width));

    lines
}

fn worker_block_lines(workers: &[WorkerOverview], width: usize) -> Vec<String> {
    if workers.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::with_capacity(workers.len() + 1);
    lines.push(format!("{}", "Workers".themed(AuraStyle::Heading)));

    for worker in workers {
        let text = match &worker.model {
            Some(model) => Cow::Owned(format!("{} ({model})", worker.description)),
            None => Cow::Borrowed(worker.description.as_str()),
        };
        // Width of the "  • name — " prefix.
        let prefix_len = INDENT.len() + 2 + worker.name.chars().count() + 3;

        if width >= prefix_len + MIN_DESC_WIDTH {
            let wrapped = wrap_words(&text, width - prefix_len);
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
        } else {
            // A full-width hang would overflow the terminal and wrap into blank
            // rows, so the name takes its own line with the description indented.
            lines.push(format!(
                "{INDENT}{} {}",
                "•".themed(AuraStyle::Muted),
                worker.name.as_str().themed(AuraStyle::Heading),
            ));
            let desc_indent = INDENT.len() + 2;
            let pad = " ".repeat(desc_indent);
            for line in wrap_words(&text, width.saturating_sub(desc_indent)) {
                lines.push(format!("{pad}{}", line.as_str().themed(AuraStyle::Muted)));
            }
        }
    }

    lines
}

/// A known-empty server set gets the setup nudge; `None` (older server) falls
/// back to the generic prompt.
fn startup_cta_lines(agent: &AgentInfo, width: usize) -> Vec<String> {
    let text = match agent.mcp_servers.as_ref() {
        Some(servers) if servers.is_empty() => {
            "Set up an MCP server to connect AURA to your tools."
        }
        _ => "What should we work on?",
    };
    wrap_words(text, width)
        .into_iter()
        .map(|line| format!("{}", line.themed(AuraStyle::Heading)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{agent, worker};
    use aura_events::McpServerOverview;
    use std::collections::BTreeMap;

    /// Strip SGR sequences (`ESC[…m`) so frame assertions are theme-independent.
    fn strip_sgr(s: &str) -> String {
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

    fn plain(lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| strip_sgr(l)).collect()
    }

    #[test]
    fn agent_overview_renders_single_agent_model_without_workers() {
        let agent = agent("ui-orch", Vec::new());
        let all = agent_overview_block_lines(&agent, 80).join("\n");

        assert!(all.contains("Agent"));
        assert!(all.contains("ui-orch"));
        assert!(all.contains("model"));
        assert!(all.contains("gpt-4o"));
        assert!(!all.contains("Workers"));
    }

    #[test]
    fn agent_overview_renders_coordinator_and_workers() {
        let agent = agent("ui-orch", vec![worker("planner"), worker("writer")]);
        let all = agent_overview_block_lines(&agent, 80).join("\n");

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
        let all = worker_block_lines(&[planner, worker("writer")], 80).join("\n");
        assert!(all.contains("(gpt-4o-mini)"));
        assert_eq!(all.matches('(').count(), 1);
    }

    #[test]
    fn wide_worker_layout_is_inline_and_exact() {
        let lines = plain(&worker_block_lines(&[worker("planner")], 80));
        assert_eq!(lines, ["Workers", "  • planner — planner does work"]);
    }

    /// Short worker names plus one 47-char name, to exercise both layout branches.
    fn mixed_width_agent() -> AgentInfo {
        agent(
            "sre-orch",
            vec![
                worker("db"),
                worker("logs"),
                worker("production-observability-correlation-specialist"),
            ],
        )
    }

    #[test]
    fn overview_frame_at_60_cols_wraps_only_the_long_name() {
        let lines = plain(&agent_overview_block_lines(&mixed_width_agent(), 60));
        assert_eq!(
            lines,
            [
                "Agent",
                "  • sre-orch — coordinator: gpt-4o",
                "Workers",
                "  • db — db does work",
                "  • logs — logs does work",
                "  • production-observability-correlation-specialist",
                "    production-observability-correlation-specialist does",
                "    work",
            ]
        );
    }

    #[test]
    fn overview_frame_at_120_cols_is_fully_inline() {
        let lines = plain(&agent_overview_block_lines(&mixed_width_agent(), 120));
        assert_eq!(
            lines,
            [
                "Agent",
                "  • sre-orch — coordinator: gpt-4o",
                "Workers",
                "  • db — db does work",
                "  • logs — logs does work",
                "  • production-observability-correlation-specialist — \
                 production-observability-correlation-specialist does work",
            ]
        );
    }

    #[test]
    fn overview_frame_is_unchanged_past_120_cols() {
        // No worker line reaches 120 cols, so 160 wraps identically.
        let agent = mixed_width_agent();
        assert_eq!(
            plain(&agent_overview_block_lines(&agent, 160)),
            plain(&agent_overview_block_lines(&agent, 120)),
        );
    }

    #[test]
    fn narrow_long_name_does_not_create_zero_width_hang() {
        // 47-char name: at 40 cols the inline prefix alone exceeds the terminal.
        let long = "production-observability-correlation-specialist";
        let lines = plain(&worker_block_lines(&[worker(long)], 40));

        // A hanging indent padded to or beyond the terminal width re-wraps into
        // blank rows, so no line may be indented that far.
        for line in &lines {
            let indent = line.chars().take_while(|c| *c == ' ').count();
            assert!(indent < 40, "over-wide indent {indent}: {line:?}");
        }
        // The name sits on its own line (no inline "—  description" tail).
        assert!(lines.iter().any(|l| l.contains(long) && !l.contains('—')));
        // The description still renders, wrapped under a small indent.
        let joined = lines.join("\n");
        assert!(joined.contains("does") && joined.contains("work"));
    }

    #[test]
    fn cta_guides_setup_when_zero_configured_mcp_servers() {
        let mut agent = agent("orch", vec![worker("planner")]);
        agent.mcp_servers = Some(BTreeMap::new());
        let lines = plain(&startup_cta_lines(&agent, 80));
        assert_eq!(
            lines.join(" "),
            "Set up an MCP server to connect AURA to your tools."
        );
    }

    #[test]
    fn cta_is_generic_when_configured_or_unknown() {
        let mut configured = agent("orch", Vec::new());
        let mut servers = BTreeMap::new();
        servers.insert(
            "logs".to_string(),
            McpServerOverview::Sse {
                url: "https://logs.example.com/sse".to_string(),
                description: None,
            },
        );
        configured.mcp_servers = Some(servers);

        // `None` (older server, mcp_servers absent) also gets the generic CTA.
        let unknown = agent("orch", Vec::new());

        for agent in [configured, unknown] {
            let lines = plain(&startup_cta_lines(&agent, 80));
            assert_eq!(lines.join(" "), "What should we work on?");
        }
    }
}
