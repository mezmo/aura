//! Format-preserving writes to an agent config file.
//!
//! The MCP install helper (and any other tooling that mutates a user's
//! `config.toml`) must not destroy unrelated settings, comments, or
//! `{{ env.VAR }}` placeholders. This module edits the raw TOML text via
//! `toml_edit` instead of round-tripping through [`crate::Config`], so
//! everything outside the touched `[mcp]` tables survives byte-for-byte.

use std::fs;
use std::path::Path;

use crate::config::McpServerConfig;
use crate::error::ConfigError;

/// Map-valued fields of an MCP server table.
const MAP_FIELDS: [&str; 4] = ["env", "headers", "headers_from_request", "scratchpad"];

/// Insert or replace `[mcp.servers.<name>]` in the config file at `path`.
///
/// The file is rewritten atomically (temp file + rename in the same
/// directory), carrying the original file's permissions over — agent
/// configs hold credentials, so a `chmod 600` must survive the rewrite.
/// A missing file is an error rather than an implicit create: a config
/// that holds only an MCP table is not runnable, so the caller is
/// expected to target an existing config. On any error the original
/// file is left untouched and the temp file removed.
pub fn upsert_mcp_server(
    path: &Path,
    name: &str,
    server: &McpServerConfig,
) -> Result<(), ConfigError> {
    let existing = fs::read_to_string(path)?;
    let updated = upsert_mcp_server_in_str(&existing, name, server)?;
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
/// `[mcp]` and `[mcp.servers]` are created as implicit tables when absent,
/// so a fresh insert emits only the `[mcp.servers.<name>]` header; when one
/// of them exists as an *inline* table (`mcp = { ... }`), it is restyled to
/// a header table so the new server can nest under it. Replacing an existing
/// server keeps the comments directly above its header (they live in the
/// table's decor) but drops comments inside the replaced table. Errors
/// (rather than clobbering) when `content` is not valid TOML or when an
/// existing `mcp`/`servers` key holds something other than a table.
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
///
/// Serde emits the `transport` tag first and struct fields in declaration
/// order, which is already deterministic. Two cleanups on top of that keep
/// the written config tidy and stable: empty optional collections (`args`
/// plus the [`MAP_FIELDS`]) are omitted entirely, and map-valued fields are
/// key-sorted so `HashMap` iteration order never leaks into the file.
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

/// Get `parent[key]` as a mutable table, creating it as an *implicit* table
/// when absent (implicit tables render no header of their own, so creating
/// `[mcp]`/`[mcp.servers]` here adds no visible lines to the file). An
/// existing inline table is converted to a header table in place — inline
/// tables can't be extended with `[header]`-style children, and the loader
/// accepts both spellings, so the data is equivalent. Errors when an
/// existing `key` holds a non-table value (including `[[array]]` tables).
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
