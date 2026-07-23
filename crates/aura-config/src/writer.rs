//! Format-preserving writes to an agent config file.
//!
//! Edits raw TOML text via `toml_edit` (no round-trip through
//! [`crate::Config`]), so unrelated settings, comments, and
//! `{{ env.VAR }}` placeholders survive untouched.

use std::fs;
use std::path::Path;

use crate::config::McpServerConfig;
use crate::error::ConfigError;

/// Map-valued fields of an MCP server table.
const MAP_FIELDS: [&str; 4] = ["env", "headers", "headers_from_request", "scratchpad"];

/// Insert or replace `[mcp.servers.<name>]` in the config file at `path`,
/// atomically (see [`rewrite_file`]). A missing file is an error, not an
/// implicit create — the caller targets an existing config.
pub fn upsert_mcp_server(
    path: &Path,
    name: &str,
    server: &McpServerConfig,
) -> Result<(), ConfigError> {
    rewrite_file(path, |existing| {
        upsert_mcp_server_in_str(existing, name, server)
    })
}

/// Append `entries` to `[orchestration.worker.<worker>].mcp_filter` in the
/// config file at `path`, with the same atomic-write guarantees as
/// [`upsert_mcp_server`].
pub fn append_worker_mcp_filter(
    path: &Path,
    worker: &str,
    entries: &[String],
) -> Result<(), ConfigError> {
    rewrite_file(path, |existing| {
        append_worker_mcp_filter_in_str(existing, worker, entries)
    })
}

/// String-level core of [`append_worker_mcp_filter`]: returns the updated
/// TOML text.
///
/// Never creates workers; dedupes entries; creates a missing `mcp_filter`
/// array. Note the semantics: writing the array narrows an absent-filter
/// (all-tools) worker, and empty `entries` writes the explicit no-tools
/// `mcp_filter = []`.
pub fn append_worker_mcp_filter_in_str(
    content: &str,
    worker: &str,
    entries: &[String],
) -> Result<String, ConfigError> {
    let mut doc: toml_edit::DocumentMut = content.parse()?;
    let worker_table = doc
        .get_mut("orchestration")
        .and_then(|o| o.get_mut("worker"))
        .and_then(|w| w.get_mut(worker))
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or_else(|| {
            ConfigError::Validation(format!(
                "no [orchestration.worker.{worker}] table in the config"
            ))
        })?;
    if worker_table.get("mcp_filter").is_none() {
        worker_table.insert(
            "mcp_filter",
            toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())),
        );
    }
    let filter = worker_table
        .get_mut("mcp_filter")
        .and_then(toml_edit::Item::as_array_mut)
        .ok_or_else(|| {
            ConfigError::Validation(format!(
                "`mcp_filter` for worker `{worker}` is not a TOML array"
            ))
        })?;
    let existing: Vec<String> = filter
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    for entry in entries {
        if !existing.contains(entry) {
            filter.push(entry.as_str());
        }
    }
    Ok(doc.to_string())
}

/// Apply a text transformation to `path` and write the result atomically
/// (temp + rename), preserving the file's permissions (configs holding
/// credentials may be chmod 600). On error the original is untouched and
/// the temp file removed.
fn rewrite_file(
    path: &Path,
    transform: impl FnOnce(&str) -> Result<String, ConfigError>,
) -> Result<(), ConfigError> {
    let existing = fs::read_to_string(path)?;
    let updated = transform(&existing)?;
    let permissions = fs::metadata(path)?.permissions();
    let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    let written = fs::write(&tmp, updated)
        .and_then(|()| fs::set_permissions(&tmp, permissions))
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// String-level core of [`upsert_mcp_server`]: returns the updated TOML text.
///
/// `[mcp]`/`[mcp.servers]` are created implicitly when absent (and restyled
/// from inline tables); replacing a server keeps the comments above its
/// header but drops those inside it. Errors (rather than clobbering) on
/// invalid TOML or a non-table `mcp`/`servers` key.
pub fn upsert_mcp_server_in_str(
    content: &str,
    name: &str,
    server: &McpServerConfig,
) -> Result<String, ConfigError> {
    if name.trim().is_empty() {
        return Err(ConfigError::Validation(
            "MCP server name must be non-empty".to_string(),
        ));
    }
    let mut doc: toml_edit::DocumentMut = content.parse()?;
    let mut table = server_to_table(server)?;
    let mcp = ensure_table(doc.as_table_mut(), "mcp")?;
    let servers = ensure_table(mcp, "servers")?;
    if let Some(toml_edit::Item::Table(old)) = servers.get(name) {
        *table.decor_mut() = old.decor().clone();
    }
    servers.insert(name, toml_edit::Item::Table(table));
    Ok(doc.to_string())
}

/// Serialize a server config into an explicit-header `toml_edit` table.
/// Empty optional collections (`args` plus the [`MAP_FIELDS`]) are omitted
/// and map-valued fields key-sorted, so `HashMap` iteration order never
/// leaks into the file.
fn server_to_table(server: &McpServerConfig) -> Result<toml_edit::Table, ConfigError> {
    let mut doc = toml_edit::ser::to_document(server)?;
    let mut table = std::mem::take(doc.as_table_mut());

    for key in MAP_FIELDS.iter().chain(std::iter::once(&"args")) {
        if item_is_empty_collection(table.get(key)) {
            table.remove(key);
        }
    }
    for key in MAP_FIELDS {
        match table.get_mut(key) {
            Some(toml_edit::Item::Table(t)) => t.sort_values(),
            Some(toml_edit::Item::Value(toml_edit::Value::InlineTable(t))) => t.sort_values(),
            _ => {}
        }
    }

    table.set_implicit(false);
    Ok(table)
}

fn item_is_empty_collection(item: Option<&toml_edit::Item>) -> bool {
    match item {
        Some(toml_edit::Item::Table(t)) => t.is_empty(),
        Some(toml_edit::Item::Value(toml_edit::Value::InlineTable(t))) => t.is_empty(),
        Some(toml_edit::Item::Value(toml_edit::Value::Array(a))) => a.is_empty(),
        _ => false,
    }
}

/// Get `parent[key]` as a mutable table, creating it implicitly (no header
/// line of its own) when absent, and converting an existing inline table in
/// place — inline tables can't take `[header]`-style children. Errors when
/// `key` holds anything else (including `[[array]]` tables).
fn ensure_table<'a>(
    parent: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, ConfigError> {
    match parent.get_mut(key) {
        None => {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            parent.insert(key, toml_edit::Item::Table(table));
        }
        Some(item) => {
            if let toml_edit::Item::Value(toml_edit::Value::InlineTable(_)) = item {
                let toml_edit::Item::Value(toml_edit::Value::InlineTable(inline)) =
                    std::mem::replace(item, toml_edit::Item::None)
                else {
                    unreachable!("matched an inline table above");
                };
                *item = toml_edit::Item::Table(inline.into_table());
            }
        }
    }
    parent
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| {
            ConfigError::Validation(format!(
                "cannot add MCP server: existing `{key}` entry is not a TOML table"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;
    use crate::load_config_from_str;
    use std::collections::HashMap;

    const BASE_CONFIG: &str = r#"# My aura config
[agent]
name = "Minimal Agent"
system_prompt = "Basic prompt"

[agent.llm]
provider = "anthropic"
api_key = "test_key"
model = "claude-3-sonnet-20240229"
"#;

    fn stdio_server() -> McpServerConfig {
        McpServerConfig::Stdio {
            cmd: vec!["npx".to_owned(), "-y".to_owned(), "some-mcp".to_owned()],
            args: vec![],
            env: HashMap::new(),
            description: None,
            scratchpad: HashMap::new(),
        }
    }

    fn http_server(headers: HashMap<String, String>) -> McpServerConfig {
        McpServerConfig::HttpStreamable {
            url: "https://mcp.example.com/mcp".to_owned(),
            headers,
            description: Some("Example server".to_owned()),
            headers_from_request: HashMap::new(),
            scratchpad: HashMap::new(),
        }
    }

    #[test]
    fn insert_preserves_original_and_creates_implicit_tables() {
        let updated = upsert_mcp_server_in_str(BASE_CONFIG, "k8s", &stdio_server()).unwrap();
        assert!(
            updated.starts_with(BASE_CONFIG),
            "original content must survive verbatim as a prefix:\n{updated}"
        );
        assert!(updated.contains("[mcp.servers.k8s]"), "{updated}");
        assert!(
            !updated.contains("\n[mcp]\n") && !updated.contains("\n[mcp.servers]\n"),
            "intermediate tables must stay implicit (no bare headers):\n{updated}"
        );
    }

    #[test]
    fn round_trips_through_loader_for_all_transports() {
        let servers: [(&str, McpServerConfig); 3] = [
            ("stdio_srv", stdio_server()),
            ("http_srv", http_server(HashMap::new())),
            (
                "sse_srv",
                McpServerConfig::Sse {
                    url: "https://sse.example.com/mcp".to_owned(),
                    headers: HashMap::new(),
                    description: None,
                    headers_from_request: HashMap::new(),
                    scratchpad: HashMap::new(),
                },
            ),
        ];
        let mut content = BASE_CONFIG.to_owned();
        for (name, server) in &servers {
            content = upsert_mcp_server_in_str(&content, name, server).unwrap();
        }
        let config = load_config_from_str(&content).expect("written config must parse");
        let parsed = config.mcp.expect("mcp table present").servers;
        assert_eq!(parsed.len(), 3);
        match &parsed["stdio_srv"] {
            McpServerConfig::Stdio { cmd, .. } => {
                assert_eq!(cmd, &["npx", "-y", "some-mcp"]);
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        match &parsed["http_srv"] {
            McpServerConfig::HttpStreamable {
                url, description, ..
            } => {
                assert_eq!(url, "https://mcp.example.com/mcp");
                assert_eq!(description.as_deref(), Some("Example server"));
            }
            other => panic!("expected http_streamable, got {other:?}"),
        }
        match &parsed["sse_srv"] {
            McpServerConfig::Sse { url, .. } => assert_eq!(url, "https://sse.example.com/mcp"),
            other => panic!("expected sse, got {other:?}"),
        }
    }

    #[test]
    fn overwrite_replaces_server_and_preserves_siblings() {
        let mut content = BASE_CONFIG.to_owned();
        content.push_str(
            r#"
# Keep me: sibling server comment
[mcp.servers.keeper]
transport = "stdio"
cmd = ["keeper-mcp"]

[mcp.servers.target]
transport = "http_streamable"
url = "https://old.example.com/mcp"
"#,
        );
        let updated =
            upsert_mcp_server_in_str(&content, "target", &http_server(HashMap::new())).unwrap();
        assert!(
            updated.contains("# Keep me: sibling server comment"),
            "{updated}"
        );
        assert!(updated.contains("cmd = [\"keeper-mcp\"]"), "{updated}");
        assert!(updated.contains("https://mcp.example.com/mcp"), "{updated}");
        assert!(
            !updated.contains("https://old.example.com/mcp"),
            "{updated}"
        );
    }

    #[test]
    fn preserves_env_placeholders_verbatim() {
        let headers = HashMap::from([(
            "Authorization".to_owned(),
            "Bearer {{ env.MEZMO_API_KEY }}".to_owned(),
        )]);
        let updated =
            upsert_mcp_server_in_str(BASE_CONFIG, "mezmo", &http_server(headers)).unwrap();
        assert!(
            updated.contains("Bearer {{ env.MEZMO_API_KEY }}"),
            "placeholder must land in the file unresolved:\n{updated}"
        );
    }

    #[test]
    fn omits_empty_optional_collections() {
        let updated = upsert_mcp_server_in_str(BASE_CONFIG, "k8s", &stdio_server()).unwrap();
        let server_section = updated.split("[mcp.servers.k8s]").nth(1).unwrap();
        for absent in ["args", "env", "scratchpad", "description"] {
            assert!(
                !server_section.contains(absent),
                "`{absent}` should be omitted when empty/None:\n{server_section}"
            );
        }
    }

    #[test]
    fn map_output_is_key_sorted_and_deterministic() {
        let headers = HashMap::from([
            ("b-header".to_owned(), "2".to_owned()),
            ("a-header".to_owned(), "1".to_owned()),
            ("c-header".to_owned(), "3".to_owned()),
        ]);
        let first =
            upsert_mcp_server_in_str(BASE_CONFIG, "srv", &http_server(headers.clone())).unwrap();
        let second = upsert_mcp_server_in_str(BASE_CONFIG, "srv", &http_server(headers)).unwrap();
        assert_eq!(first, second);
        let a = first.find("a-header").unwrap();
        let b = first.find("b-header").unwrap();
        let c = first.find("c-header").unwrap();
        assert!(a < b && b < c, "headers must be key-sorted:\n{first}");
    }

    #[test]
    fn preserves_comment_above_replaced_server_header() {
        let content = format!(
            "{BASE_CONFIG}\n# IMPORTANT: do not remove\n[mcp.servers.target]\ntransport = \"stdio\"\ncmd = [\"old\"]\n"
        );
        let updated =
            upsert_mcp_server_in_str(&content, "target", &http_server(HashMap::new())).unwrap();
        assert!(
            updated.contains("# IMPORTANT: do not remove"),
            "comment above the replaced header must survive:\n{updated}"
        );
        assert!(!updated.contains("cmd = [\"old\"]"), "{updated}");
    }

    #[test]
    fn converts_inline_servers_table_to_header_table() {
        let content = format!(
            "{BASE_CONFIG}\n[mcp]\nservers = {{ target = {{ transport = \"stdio\", cmd = [\"old\"] }} }}\n"
        );
        let updated =
            upsert_mcp_server_in_str(&content, "target", &http_server(HashMap::new())).unwrap();
        let config = load_config_from_str(&updated).expect("restyled config must parse");
        let parsed = config.mcp.expect("mcp table present").servers;
        match &parsed["target"] {
            McpServerConfig::HttpStreamable { url, .. } => {
                assert_eq!(url, "https://mcp.example.com/mcp");
            }
            other => panic!("expected http_streamable, got {other:?}"),
        }
    }

    #[test]
    fn upserts_next_to_dotted_key_server() {
        let content = format!(
            "{BASE_CONFIG}\n[mcp]\nservers.dotted.transport = \"stdio\"\nservers.dotted.cmd = [\"dotted-mcp\"]\n"
        );
        let updated =
            upsert_mcp_server_in_str(&content, "added", &http_server(HashMap::new())).unwrap();
        let config = load_config_from_str(&updated).expect("config must parse");
        let parsed = config.mcp.expect("mcp table present").servers;
        assert_eq!(parsed.len(), 2);
        assert!(
            matches!(&parsed["dotted"], McpServerConfig::Stdio { cmd, .. } if cmd == &["dotted-mcp"])
        );
    }

    #[test]
    fn round_trips_nonempty_maps_through_loader() {
        let server = McpServerConfig::HttpStreamable {
            url: "https://mcp.example.com/mcp".to_owned(),
            headers: HashMap::from([("X-Static".to_owned(), "s".to_owned())]),
            description: None,
            headers_from_request: HashMap::from([(
                "Authorization".to_owned(),
                "authorization".to_owned(),
            )]),
            scratchpad: HashMap::new(),
        };
        let updated = upsert_mcp_server_in_str(BASE_CONFIG, "srv", &server).unwrap();
        let config = load_config_from_str(&updated).expect("config must parse");
        match &config.mcp.expect("mcp table present").servers["srv"] {
            McpServerConfig::HttpStreamable {
                headers,
                headers_from_request,
                ..
            } => {
                assert_eq!(headers.get("X-Static").map(String::as_str), Some("s"));
                assert_eq!(
                    headers_from_request
                        .get("Authorization")
                        .map(String::as_str),
                    Some("authorization")
                );
            }
            other => panic!("expected http_streamable, got {other:?}"),
        }
    }

    #[test]
    fn errors_on_array_of_tables_servers() {
        let content = format!("{BASE_CONFIG}\n[[mcp.servers]]\ntransport = \"stdio\"\n");
        let err = upsert_mcp_server_in_str(&content, "srv", &stdio_server()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "{err:?}");
    }

    #[test]
    fn errors_on_malformed_toml() {
        let err = upsert_mcp_server_in_str("not = [valid", "srv", &stdio_server()).unwrap_err();
        assert!(matches!(err, ConfigError::TomlEdit(_)), "{err:?}");
    }

    #[test]
    fn errors_on_empty_name() {
        let err = upsert_mcp_server_in_str(BASE_CONFIG, "  ", &stdio_server()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "{err:?}");
    }

    #[test]
    fn errors_when_mcp_is_not_a_table() {
        let content = format!("mcp = 3\n{BASE_CONFIG}");
        let err = upsert_mcp_server_in_str(&content, "srv", &stdio_server()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "{err:?}");
    }

    const ORCHESTRATED_CONFIG: &str = r#"[agent]
name = "Coordinator"
system_prompt = "Coordinate"

[agent.llm]
provider = "anthropic"
api_key = "test_key"
model = "claude-3-sonnet-20240229"

[orchestration]
enabled = true

# Ops worker comment
[orchestration.worker.operations]
description = "ops"
preamble = "You are ops"
mcp_filter = ["logs_*"] # trailing comment

[orchestration.worker.writer]
description = "writes summaries"
preamble = "You write"
"#;

    #[test]
    fn appends_filter_entries_and_dedupes() {
        let updated = append_worker_mcp_filter_in_str(
            ORCHESTRATED_CONFIG,
            "operations",
            &["mezmo_*".to_string(), "logs_*".to_string()],
        )
        .unwrap();
        assert!(
            updated.contains(r#"mcp_filter = ["logs_*", "mezmo_*"]"#),
            "existing entry deduped, new one appended:\n{updated}"
        );
        assert!(updated.contains("# Ops worker comment"), "{updated}");
        let config = load_config_from_str(&updated).expect("config must parse");
        let orch = config.orchestration.expect("orchestration table");
        assert_eq!(
            orch.workers["operations"].mcp_filter.as_deref(),
            Some(["logs_*".to_string(), "mezmo_*".to_string()].as_slice())
        );
    }

    #[test]
    fn creates_missing_filter_array() {
        let updated =
            append_worker_mcp_filter_in_str(ORCHESTRATED_CONFIG, "writer", &["k8s_*".to_string()])
                .unwrap();
        let config = load_config_from_str(&updated).expect("config must parse");
        assert_eq!(
            config.orchestration.expect("orchestration table").workers["writer"]
                .mcp_filter
                .as_deref(),
            Some(["k8s_*".to_string()].as_slice())
        );
    }

    #[test]
    fn empty_entries_write_the_no_tools_assignment() {
        let updated = append_worker_mcp_filter_in_str(ORCHESTRATED_CONFIG, "writer", &[]).unwrap();
        assert!(updated.contains("mcp_filter = []"), "{updated}");
        let config = load_config_from_str(&updated).expect("config must parse");
        assert_eq!(
            config.orchestration.expect("orchestration table").workers["writer"].mcp_filter,
            Some(vec![])
        );
    }

    #[test]
    fn errors_on_unknown_worker() {
        let err =
            append_worker_mcp_filter_in_str(ORCHESTRATED_CONFIG, "nope", &["k8s_*".to_string()])
                .unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)), "{err:?}");
    }

    #[test]
    fn file_append_worker_filter_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, ORCHESTRATED_CONFIG).unwrap();
        append_worker_mcp_filter(&path, "operations", &["mezmo_*".to_string()]).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains(r#"["logs_*", "mezmo_*"]"#), "{written}");
    }

    #[test]
    fn file_upsert_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, BASE_CONFIG).unwrap();

        upsert_mcp_server(&path, "k8s", &stdio_server()).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[mcp.servers.k8s]"));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_upsert_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, BASE_CONFIG).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        upsert_mcp_server(&path, "k8s", &stdio_server()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "permissions must survive the rewrite");
    }

    #[test]
    fn file_upsert_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            upsert_mcp_server(&dir.path().join("nope.toml"), "srv", &stdio_server()).unwrap_err();
        assert!(matches!(err, ConfigError::Io(_)), "{err:?}");
    }

    #[test]
    fn file_upsert_leaves_file_untouched_on_malformed_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = [valid").unwrap();
        upsert_mcp_server(&path, "srv", &stdio_server()).unwrap_err();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid");
    }
}
