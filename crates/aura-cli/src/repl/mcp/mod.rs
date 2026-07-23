//! `/mcp` — inspect the MCP servers available to the active agent.
//!
//! Reads the same credential-free [`AgentInfo::mcp_servers`] view the
//! startup call-to-action uses, so it works identically against a local
//! standalone agent and a remote aura-web-server.

#[cfg(feature = "standalone-cli")]
mod catalog;
#[cfg(feature = "standalone-cli")]
mod wizard;

use aura_events::{AgentInfo, McpServerOverview};

use super::registry::CommandContext;
use crate::theme::{AuraStyle, Themed};
use crate::ui::prompt::redraw_input_frame;

pub(crate) fn handle_mcp(ctx: &mut CommandContext, args: &str) {
    match args.trim() {
        "" => {
            let agent = ctx.rt.block_on(ctx.backend.startup_agent_overview());
            println!("{}", format_server_list(agent.as_ref()));
        }
        "add" => {
            #[cfg(feature = "standalone-cli")]
            wizard::run(ctx);
            #[cfg(not(feature = "standalone-cli"))]
            println!(
                "/mcp add edits a local agent config and needs a build with the \
                 standalone-cli feature; this build is HTTP-only."
            );
        }
        other => println!(
            "Unknown /mcp subcommand: {other}\n\
             Run /mcp to list configured servers, or /mcp add to set one up."
        ),
    }
    redraw_input_frame();
}

/// A known-empty server set gets the setup nudge, mirroring the startup CTA;
/// `None` means the connected server predates the `mcp_servers` info field.
fn format_server_list(agent: Option<&AgentInfo>) -> String {
    let Some(agent) = agent else {
        return "No agent information available.".to_string();
    };
    let Some(servers) = agent.mcp_servers.as_ref() else {
        return "The connected server does not report MCP configuration.".to_string();
    };
    if servers.is_empty() {
        return format!(
            "No MCP servers configured for {}.\nSet up an MCP server to connect AURA to your tools.",
            agent.id
        );
    }

    let mut lines = vec![format!(
        "{} {}",
        "MCP servers".themed(AuraStyle::Heading),
        format!("({})", agent.id).themed(AuraStyle::Muted),
    )];
    for (name, server) in servers {
        let (transport, target, description): (&str, &str, Option<&str>) = match server {
            McpServerOverview::Stdio {
                command,
                description,
            } => ("stdio", command, description.as_deref()),
            McpServerOverview::HttpStreamable { url, description } => {
                ("http_streamable", url, description.as_deref())
            }
            McpServerOverview::Sse { url, description } => ("sse", url, description.as_deref()),
            // `McpServerOverview` is #[non_exhaustive]; a newer wire peer
            // may send a transport this build doesn't know.
            _ => ("unknown transport", "", None),
        };
        lines.push(format!(
            "  {} {} {} {} {}",
            "•".themed(AuraStyle::Muted),
            name.as_str().themed(AuraStyle::Heading),
            "—".themed(AuraStyle::Muted),
            transport.themed(AuraStyle::Muted),
            target.themed(AuraStyle::Identifier),
        ));
        if let Some(desc) = description {
            lines.push(format!("      {}", desc.themed(AuraStyle::Muted)));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Strip SGR sequences (`ESC[…m`) so assertions are theme-independent.
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

    #[test]
    fn lists_servers_with_transport_and_target() {
        let mut agent = crate::test_fixtures::agent("sre", vec![]);
        agent.mcp_servers = Some(BTreeMap::from([
            (
                "mezmo".to_string(),
                McpServerOverview::HttpStreamable {
                    url: "https://mcp.mezmo.com".to_string(),
                    description: Some("Log analysis".to_string()),
                },
            ),
            (
                "k8s".to_string(),
                McpServerOverview::Stdio {
                    command: "kubernetes-mcp-server".to_string(),
                    description: None,
                },
            ),
        ]));
        let text = strip_sgr(&format_server_list(Some(&agent)));
        assert!(text.contains("MCP servers (sre)"), "{text}");
        assert!(
            text.contains("mezmo — http_streamable https://mcp.mezmo.com"),
            "{text}"
        );
        assert!(text.contains("Log analysis"), "{text}");
        assert!(text.contains("k8s — stdio kubernetes-mcp-server"), "{text}");
    }

    #[test]
    fn empty_set_nudges_setup() {
        let mut agent = crate::test_fixtures::agent("sre", vec![]);
        agent.mcp_servers = Some(BTreeMap::new());
        let text = format_server_list(Some(&agent));
        assert!(text.contains("No MCP servers configured for sre"), "{text}");
    }

    #[test]
    fn absent_field_reports_older_server() {
        let agent = crate::test_fixtures::agent("sre", vec![]);
        let text = format_server_list(Some(&agent));
        assert!(text.contains("does not report MCP configuration"), "{text}");
    }
}
