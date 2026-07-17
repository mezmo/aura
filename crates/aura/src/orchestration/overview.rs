//! Projections of an agent [`Config`] into the wire types served by
//! `GET /aura/info` ([`AgentInfo`], [`WorkerOverview`]).

use aura_config::{Config, McpServerConfig};
use aura_events::{AgentInfo, McpServerOverview, WorkerOverview};

pub fn agent_info(config: &Config) -> AgentInfo {
    AgentInfo {
        id: config.agent_id().to_owned(),
        model: config.agent.llm.model_info().1.to_owned(),
        workers: worker_overview(config),
        // `Some(empty)` means this config has no servers; `None` is reserved
        // for older servers that omit the field.
        mcp_servers: Some(
            config
                .mcp
                .as_ref()
                .map(|mcp| {
                    mcp.servers
                        .iter()
                        .map(|(name, server)| (name.clone(), mcp_server_overview(server)))
                        .collect()
                })
                .unwrap_or_default(),
        ),
    }
}

/// Credential stripping is transport-level: [`sanitize_url`] reduces URLs to
/// their origin, and `headers`/`env`/`headers_from_request`/`args` are dropped.
/// Stdio keeps only the executable basename, so command-line secrets never
/// reach the wire.
fn mcp_server_overview(server: &McpServerConfig) -> McpServerOverview {
    match server {
        McpServerConfig::Stdio {
            cmd, description, ..
        } => McpServerOverview::Stdio {
            command: command_basename(cmd),
            description: description.clone(),
        },
        McpServerConfig::HttpStreamable {
            url, description, ..
        } => McpServerOverview::HttpStreamable {
            url: sanitize_url(url),
            description: description.clone(),
        },
        McpServerConfig::Sse {
            url, description, ..
        } => McpServerOverview::Sse {
            url: sanitize_url(url),
            description: description.clone(),
        },
    }
}

/// Reduce a URL to its origin (`scheme://host[:port]`). Userinfo, path, query,
/// and fragment can all carry secrets — path-embedded tokens are a common MCP
/// hosting pattern — so none of them survive. Input that won't parse, or that
/// has no tuple origin (cannot-be-a-base or hostless URLs), has no safe form
/// and collapses to `<invalid url>`.
fn sanitize_url(raw: &str) -> String {
    let Ok(url) = url::Url::parse(raw) else {
        return "<invalid url>".to_string();
    };
    match url.origin() {
        origin @ url::Origin::Tuple(..) => origin.ascii_serialization(),
        url::Origin::Opaque(_) => "<invalid url>".to_string(),
    }
}

/// Basename of the executable (first `cmd` element). Splits on both `/` and
/// `\` so a foreign-platform path never survives as one component — the
/// directory part of a command path can carry secrets. `<unknown>` when the
/// command is empty or has no real file name.
fn command_basename(cmd: &[String]) -> String {
    cmd.first()
        .and_then(|program| program.rsplit(['/', '\\']).next())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .map_or_else(|| "<unknown>".to_string(), str::to_owned)
}

/// Summarize a config's orchestration workers, sorted by name. Empty when
/// orchestration is disabled.
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
    use super::{agent_info, command_basename, sanitize_url, worker_overview};
    use aura_config::load_config_from_str;
    use aura_events::McpServerOverview;
    use std::collections::BTreeMap;

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

    #[test]
    fn agent_info_projects_credential_free_mcp_config_view_without_connecting() {
        let base = r#"
[agent]
name = "mcp-presence"
system_prompt = "p"
[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"
"#;

        // Both absent `[mcp]` and an empty server map project to `Some(empty)`.
        for (name, mcp) in [
            ("absent mcp table", ""),
            ("empty server map", "\n[mcp]\nservers = {}\n"),
        ] {
            let config = load_config_from_str(&format!("{base}{mcp}"))
                .unwrap_or_else(|error| panic!("{name} should parse: {error}"));
            assert_eq!(
                agent_info(&config).mcp_servers,
                Some(BTreeMap::new()),
                "{name}"
            );
        }

        let config = load_config_from_str(&format!(
            "{base}{}",
            r#"
[mcp.servers.dead]
transport = "http_streamable"
url = "http://user:secret@127.0.0.1:9/s/pathsecret/mcp?token=abc"
description = "Dead server."
headers = { authorization = "Bearer topsecret" }

[mcp.servers.tool]
transport = "stdio"
cmd = ["/opt/mcp/bin/fs-server"]
args = ["--api-key", "argsecret"]
"#
        ))
        .expect("configured servers should parse");

        let servers = agent_info(&config)
            .mcp_servers
            .expect("a current server always projects Some");
        assert_eq!(
            servers["dead"],
            McpServerOverview::HttpStreamable {
                url: "http://127.0.0.1:9".to_string(),
                description: Some("Dead server.".to_string()),
            }
        );
        assert_eq!(
            servers["tool"],
            McpServerOverview::Stdio {
                command: "fs-server".to_string(),
                description: None,
            }
        );
        // No secret from headers/userinfo/path/query/args leaks into the
        // serialized view.
        let json = serde_json::to_string(&servers).unwrap();
        for secret in [
            "topsecret",
            "secret",
            "pathsecret",
            "argsecret",
            "token",
            "authorization",
            "api-key",
            "/opt/mcp",
        ] {
            assert!(!json.contains(secret), "leaked {secret}: {json}");
        }
    }

    #[test]
    fn sanitize_url_reduces_to_origin_across_forms() {
        // userinfo, path, query, and fragment all dropped; scheme/host/port kept
        assert_eq!(
            sanitize_url("http://user:secret@127.0.0.1:9/mcp?token=abc#frag"),
            "http://127.0.0.1:9"
        );
        // path-embedded token dropped with the rest of the path
        assert_eq!(
            sanitize_url("https://mcp.example.com/s/SECRET/mcp"),
            "https://mcp.example.com"
        );
        // default port elided, no trailing slash
        assert_eq!(
            sanitize_url("https://user:pass@example.com"),
            "https://example.com"
        );
        // IPv6 host and explicit port preserved
        assert_eq!(
            sanitize_url("http://user:pass@[::1]:9/mcp"),
            "http://[::1]:9"
        );
        // unparseable or origin-less input collapses to a sentinel — never a
        // partial or raw leak
        assert_eq!(sanitize_url("not a url"), "<invalid url>");
        assert_eq!(sanitize_url(""), "<invalid url>");
        assert_eq!(sanitize_url("unix:/var/run/mcp.sock"), "<invalid url>");
    }

    #[test]
    fn command_basename_extracts_or_fails_closed() {
        let cmd = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(command_basename(&cmd(&["fs-server"])), "fs-server");
        assert_eq!(
            command_basename(&cmd(&["/opt/mcp/bin/fs-server", "--api-key", "s"])),
            "fs-server"
        );
        // a foreign-platform separator still splits, so the directory part of
        // the path never reaches the wire
        assert_eq!(
            command_basename(&cmd(&["C:\\Users\\me\\secret-dir\\server.exe"])),
            "server.exe"
        );
        assert_eq!(command_basename(&[]), "<unknown>");
        assert_eq!(command_basename(&cmd(&[".."])), "<unknown>");
        assert_eq!(command_basename(&cmd(&["/opt/bin/"])), "<unknown>");
    }
}
