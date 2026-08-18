//! Projections of an agent [`Config`] into the wire types served by
//! `GET /aura/info` ([`AgentInfo`], [`WorkerOverview`]).

use aura_config::{Config, McpServerConfig};
use aura_events::{
    AgentInfo, McpServerOverview, McpToolAnnotations, McpToolOverview, WorkerOverview,
};
use std::collections::HashMap;
use std::time::Duration;

pub fn agent_info(config: &Config) -> AgentInfo {
    AgentInfo {
        id: config.agent_id().to_owned(),
        description: config.agent.description.clone(),
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

/// [`agent_info`] plus each server's live tool list, discovered by connecting
/// to every configured MCP server and issuing `tools/list`.
///
/// `req_headers` feeds `headers_from_request` resolution, so the discovered
/// tools are the ones this caller's credentials can see. Connecting is bounded
/// by `timeout`, and every client — including spawned stdio children — is
/// closed before returning.
///
/// A server sets `tools` only once it connects: one that fails, is never
/// attempted, or is still connecting when `timeout` expires reports `None`
/// rather than an empty list, so "no tools" stays distinguishable from "no
/// answer".
pub async fn agent_info_with_tools(
    config: &Config,
    req_headers: Option<&HashMap<String, String>>,
    timeout: Duration,
) -> AgentInfo {
    let mut info = agent_info(config);
    let Some(mut mcp_config) = config.mcp.clone() else {
        return info;
    };
    crate::rig_builder::resolve_mcp_headers_in(&mut mcp_config, req_headers);

    let discovered = discover_tools(&mcp_config, timeout).await;
    if let Some(servers) = info.mcp_servers.as_mut() {
        for (name, server) in servers.iter_mut() {
            set_tools(server, discovered.get(name).cloned());
        }
    }
    info
}

/// Connect to every server in `mcp_config` and collect the tools each reports,
/// keyed by server name. Servers that did not connect are absent from the map.
async fn discover_tools(
    mcp_config: &aura_config::McpConfig,
    timeout: Duration,
) -> HashMap<String, Vec<McpToolOverview>> {
    let manager = match tokio::time::timeout(
        timeout,
        crate::mcp::McpManager::initialize_from_config(mcp_config),
    )
    .await
    {
        Ok(Ok(manager)) => manager,
        Ok(Err(e)) => {
            tracing::warn!("MCP tool discovery for /aura/info failed: {e}");
            return HashMap::new();
        }
        Err(_) => {
            tracing::warn!(
                "MCP tool discovery for /aura/info timed out after {}s",
                timeout.as_secs()
            );
            return HashMap::new();
        }
    };

    let per_server = manager
        .server_info
        .iter()
        .filter(|(_, server)| matches!(server.status, crate::mcp::ConnectionStatus::Connected))
        .map(|(name, _)| {
            let tools = manager
                .streamable_tools
                .get(name)
                .or_else(|| manager.sse_tools.get(name))
                .or_else(|| manager.stdio_tools.get(name))
                .map(|tools| tools.iter().map(tool_overview).collect())
                .unwrap_or_default();
            (name.clone(), tools)
        })
        .collect();

    manager
        .cancel_and_close_all("aura-info", "tool detail collected")
        .await;
    per_server
}

/// Project a discovered MCP tool into its wire form.
///
/// These are the values AURA holds, not the ones the server advertised:
/// `McpManager::sanitize_mcp_tool` has already rewritten the name to the
/// LLM-safe character set and, under `[mcp].sanitize_schemas`, rewritten the
/// input schema. Publishing that form is what makes the output usable for
/// governance — it names the tools AURA actually invokes with the schemas the
/// model actually receives.
///
/// `icons` is dropped; everything else in the MCP `Tool` object carries over.
fn tool_overview(tool: &rmcp::model::Tool) -> McpToolOverview {
    McpToolOverview {
        name: tool.name.to_string(),
        title: tool.title.clone(),
        description: tool.description.as_ref().map(|d| d.to_string()),
        input_schema: Some(serde_json::Value::Object((*tool.input_schema).clone())),
        output_schema: tool
            .output_schema
            .as_ref()
            .map(|schema| serde_json::Value::Object((**schema).clone())),
        annotations: tool
            .annotations
            .as_ref()
            .map(|annotations| McpToolAnnotations {
                title: annotations.title.clone(),
                read_only_hint: annotations.read_only_hint,
                destructive_hint: annotations.destructive_hint,
                idempotent_hint: annotations.idempotent_hint,
                open_world_hint: annotations.open_world_hint,
            }),
        meta: tool
            .meta
            .as_ref()
            .map(|meta| serde_json::Value::Object(meta.0.clone())),
    }
}

fn set_tools(server: &mut McpServerOverview, discovered: Option<Vec<McpToolOverview>>) {
    match server {
        McpServerOverview::Stdio { tools, .. }
        | McpServerOverview::HttpStreamable { tools, .. }
        | McpServerOverview::Sse { tools, .. } => *tools = discovered,
        // `McpServerOverview` is #[non_exhaustive]; a transport added to the
        // wire crate but not handled here simply carries no tools.
        _ => {}
    }
}

/// Reduce every discovered tool to the fields a listing needs, dropping the
/// schemas that dominate the payload.
pub fn summarize_tools(info: &mut AgentInfo) {
    let Some(servers) = info.mcp_servers.as_mut() else {
        return;
    };
    for server in servers.values_mut() {
        let (McpServerOverview::Stdio { tools, .. }
        | McpServerOverview::HttpStreamable { tools, .. }
        | McpServerOverview::Sse { tools, .. }) = server
        else {
            continue;
        };
        let Some(tools) = tools.as_mut() else {
            continue;
        };
        for tool in tools.iter_mut() {
            *tool = McpToolOverview {
                name: std::mem::take(&mut tool.name),
                description: tool.description.take(),
                ..McpToolOverview::default()
            };
        }
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
            tools: None,
        },
        McpServerConfig::HttpStreamable {
            url, description, ..
        } => McpServerOverview::HttpStreamable {
            url: sanitize_url(url),
            description: description.clone(),
            tools: None,
        },
        McpServerConfig::Sse {
            url, description, ..
        } => McpServerOverview::Sse {
            url: sanitize_url(url),
            description: description.clone(),
            tools: None,
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
    use super::{
        agent_info, agent_info_with_tools, command_basename, sanitize_url, summarize_tools,
        worker_overview,
    };
    use aura_config::load_config_from_str;
    use aura_events::{AgentInfo, McpServerOverview, McpToolOverview};
    use std::collections::BTreeMap;
    use std::time::Duration;

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
    fn agent_info_carries_the_configured_description() {
        let config_with = |agent_fields: &str| {
            load_config_from_str(&format!(
                r#"
[agent]
name = "described"
system_prompt = "p"
{agent_fields}
[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"
"#
            ))
            .expect("config should parse")
        };

        assert_eq!(agent_info(&config_with("")).description, None);
        assert_eq!(
            agent_info(&config_with(
                r#"description = "Triage production incidents""#
            ))
            .description
            .as_deref(),
            Some("Triage production incidents")
        );
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
                tools: None,
            }
        );
        assert_eq!(
            servers["tool"],
            McpServerOverview::Stdio {
                command: "fs-server".to_string(),
                description: None,
                tools: None,
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

    /// Nothing about a dead server should make it look like it answered.
    #[tokio::test]
    async fn agent_info_with_tools_reports_no_tools_for_a_server_that_never_connects() {
        let config = load_config_from_str(
            r#"
[agent]
name = "unreachable"
system_prompt = "p"
[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"

[mcp.servers.dead]
transport = "http_streamable"
url = "http://user:secret@127.0.0.1:9/mcp"
description = "Dead server."
headers = { authorization = "Bearer topsecret" }
"#,
        )
        .expect("config should parse");

        let info = agent_info_with_tools(&config, None, Duration::from_secs(5)).await;

        // The config-derived projection is untouched, credentials included.
        assert_eq!(info.id, "unreachable");
        assert_eq!(
            info.mcp_servers.as_ref().expect("projects Some")["dead"],
            McpServerOverview::HttpStreamable {
                url: "http://127.0.0.1:9".to_string(),
                description: Some("Dead server.".to_string()),
                tools: None,
            },
            "a server that never connected reports no tools, not an empty list"
        );
    }

    /// An agent with no `[mcp]` table makes no connection attempt at all.
    #[tokio::test]
    async fn agent_info_with_tools_matches_agent_info_without_mcp() {
        let config = load_config_from_str(
            r#"
[agent]
name = "solo"
system_prompt = "p"
[agent.llm]
provider = "openai"
model = "gpt-4o"
api_key = "k"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            agent_info_with_tools(&config, None, Duration::from_secs(5)).await,
            agent_info(&config)
        );
    }

    #[test]
    fn summarize_tools_keeps_only_name_and_description() {
        let full = McpToolOverview {
            name: "list_incidents".to_string(),
            title: Some("List Incidents".to_string()),
            description: Some("List open incidents".to_string()),
            input_schema: Some(serde_json::json!({ "type": "object" })),
            output_schema: Some(serde_json::json!({ "type": "object" })),
            annotations: Some(aura_events::McpToolAnnotations {
                read_only_hint: Some(true),
                ..Default::default()
            }),
            meta: Some(serde_json::json!({ "k": "v" })),
        };
        let mut info = AgentInfo {
            id: "a".to_string(),
            description: None,
            model: "gpt-4o".to_string(),
            workers: Vec::new(),
            mcp_servers: Some(BTreeMap::from([
                (
                    "with-tools".to_string(),
                    McpServerOverview::Sse {
                        url: "https://mcp.example.com".to_string(),
                        description: None,
                        tools: Some(vec![full]),
                    },
                ),
                (
                    "no-tools".to_string(),
                    McpServerOverview::Stdio {
                        command: "fs-server".to_string(),
                        description: None,
                        tools: None,
                    },
                ),
            ])),
        };

        summarize_tools(&mut info);

        let servers = info.mcp_servers.expect("projects Some");
        assert_eq!(
            servers["with-tools"],
            McpServerOverview::Sse {
                url: "https://mcp.example.com".to_string(),
                description: None,
                tools: Some(vec![McpToolOverview {
                    name: "list_incidents".to_string(),
                    description: Some("List open incidents".to_string()),
                    ..Default::default()
                }]),
            }
        );
        // A server carrying no tool list is left alone.
        assert_eq!(
            servers["no-tools"],
            McpServerOverview::Stdio {
                command: "fs-server".to_string(),
                description: None,
                tools: None,
            }
        );
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
