//! By-reference tool arguments: the model names a stored file in place of a
//! large string argument, and the tool receives the file's exact contents.
//!
//! Configured per tool and field at
//! `[mcp.servers.<name>.scratchpad.by_reference]`. For each field, the tool
//! schema gains an optional `<field>_file` sibling, and [`ArgReferenceTool`]
//! swaps the referenced file's contents into `<field>` right before the tool
//! it wraps runs. Whether that is inside or outside the tool wrapper chain —
//! and so whether wrappers see the reference or the expanded content — is
//! decided in `Agent::add_mcp_tool`.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::{Tool as RigTool, ToolError};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, OnceCell};

use super::context_budget::TokenCounter;
use super::storage::ScratchpadStorage;
use crate::config::{McpConfig, glob_match};
use crate::mcp::MAX_RAW_PAYLOAD_BYTES;
use crate::orchestration::ExecutionPersistence;

pub use aura_config::{FieldPath, FieldSegment};

fn reference_key(field: &str) -> String {
    format!("{field}_file")
}

/// Note appended to a by-reference field's description.
fn reference_note(reference: &str) -> String {
    format!("Or set `{reference}` to send a stored file's contents unchanged.")
}

/// Schema of the `<field>_file` property added beside a by-reference field.
fn reference_property(field: &str, reference: &str) -> Value {
    serde_json::json!({
        "type": "string",
        "description": format!(
            "A scratchpad file or run artifact whose exact contents to send as \
             `{field}`, instead of writing the value out. Takes the file names shown \
             in `[scratchpad: ...]` and `[raw: ...]` pointers (use the raw copy when \
             there is one) and artifact filenames. Set `{field}` or `{reference}`, \
             not both."
        ),
    })
}

/// Server name → tool name → argument fields that accept a file reference.
#[derive(Debug, Clone, Default)]
pub struct ByReferenceMap(HashMap<String, HashMap<String, Vec<FieldPath>>>);

impl ByReferenceMap {
    /// Fields of `server`'s tool `tool` that accept a reference.
    pub fn get(&self, server: &str, tool: &str) -> Option<&[FieldPath]> {
        self.0.get(server)?.get(tool).map(Vec::as_slice)
    }

    /// Whether a tool named `tool` accepts a reference on any server.
    pub fn contains_tool(&self, tool: &str) -> bool {
        self.0.values().any(|tools| tools.contains_key(tool))
    }

    /// Number of (server, tool) pairs with reference fields.
    pub fn len(&self) -> usize {
        self.0.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every tool name with reference fields, with those fields (a tool name
    /// on several servers appears once per server).
    pub fn tools(&self) -> impl Iterator<Item = (&str, &[FieldPath])> {
        self.0.values().flat_map(|tools| {
            tools
                .iter()
                .map(|(tool, fields)| (tool.as_str(), fields.as_slice()))
        })
    }
}

/// Tokens that [`add_reference_fields`] adds to the schemas of tools
/// reachable through `mcp_filter` (`None` = all): each `<field>_file`
/// property plus the note on its field. Counted per configured field, so a
/// field a schema lacks is over-counted — the safe direction for seeding the
/// context budget, which otherwise counts only the servers' own schemas.
pub fn reference_twin_tokens(
    counter: &dyn TokenCounter,
    by_reference: &ByReferenceMap,
    mcp_filter: Option<&[String]>,
) -> usize {
    by_reference
        .tools()
        .filter(|(tool, _)| {
            mcp_filter.is_none_or(|filter| filter.iter().any(|p| glob_match(p, tool)))
        })
        .flat_map(|(_, fields)| fields)
        .map(|path| {
            let field = path.field();
            let reference = reference_key(field);
            counter.count_tokens(&reference)
                + counter.count_tokens(&reference_property(field, &reference).to_string())
                + counter.count_tokens(&reference_note(&reference))
        })
        .sum()
}

/// Resolve `[mcp.servers.*.scratchpad.by_reference]` patterns against each
/// server's tool list; within a server the longest matching pattern wins
/// (equal lengths: the lexicographically smaller pattern).
///
/// Unlike [`scratchpad_tool_map`](super::scratchpad_tool_map), the result
/// stays keyed by server. A reference makes aura send stored file contents
/// to the tool's server, so one server's opt-in must never reach a
/// same-named tool on another.
pub fn by_reference_map(
    mcp: Option<&McpConfig>,
    tool_names_per_server: &HashMap<String, Vec<String>>,
) -> ByReferenceMap {
    let Some(mcp) = mcp else {
        return ByReferenceMap::default();
    };

    let mut resolved = HashMap::new();
    for (server_name, tools) in tool_names_per_server {
        let Some(server_cfg) = mcp.servers.get(server_name) else {
            continue;
        };
        let patterns = &server_cfg.scratchpad().by_reference;
        let server_tools: HashMap<String, Vec<FieldPath>> = tools
            .iter()
            .filter_map(|tool_name| {
                let (_, fields) = patterns
                    .iter()
                    .filter(|(pattern, _)| glob_match(pattern, tool_name))
                    .min_by(|(pa, _), (pb, _)| pb.len().cmp(&pa.len()).then(pa.cmp(pb)))?;
                (!fields.is_empty()).then(|| (tool_name.clone(), fields.clone()))
            })
            .collect();
        if !server_tools.is_empty() {
            resolved.insert(server_name.clone(), server_tools);
        }
    }
    ByReferenceMap(resolved)
}

/// Add a `<field>_file` property beside each of `fields` in a tool's
/// parameter schema, and drop the field from its object's `required` list so
/// the model may send either. Returns the fields that were added: a field is
/// skipped when the schema doesn't declare it, or already declares a
/// `<field>_file` property of its own.
///
/// "Exactly one of the two" is enforced by [`resolve_references`] rather than
/// in the schema, since providers reject top-level `oneOf`/`anyOf`.
pub fn add_reference_fields(schema: &mut Value, fields: &[FieldPath]) -> Vec<FieldPath> {
    let mut applied = Vec::new();
    for path in fields {
        let field = path.field();
        let reference = reference_key(field);
        let Some(object) = object_schema(schema, path.parents()) else {
            tracing::debug!("by_reference: no object schema for '{path}'; skipping");
            continue;
        };
        let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) else {
            tracing::debug!("by_reference: no properties for '{path}'; skipping");
            continue;
        };
        if properties.contains_key(&reference) {
            tracing::debug!("by_reference: schema already declares '{reference}'; skipping");
            continue;
        }
        let Some(original) = properties.get_mut(field) else {
            tracing::debug!("by_reference: schema has no field '{path}'; skipping");
            continue;
        };
        if let Some(original) = original.as_object_mut() {
            let note = reference_note(&reference);
            let description = match original.get("description").and_then(Value::as_str) {
                Some(existing) => format!("{existing} {note}"),
                None => note,
            };
            original.insert("description".to_string(), Value::String(description));
        }
        properties.insert(reference.clone(), reference_property(field, &reference));
        if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| name.as_str() != Some(field));
        }
        applied.push(path.clone());
    }
    applied
}

/// The object schema that a field path's parent segments lead to.
fn object_schema<'a>(
    schema: &'a mut Value,
    parents: &[FieldSegment],
) -> Option<&'a mut Map<String, Value>> {
    let mut node = schema;
    for segment in parents {
        node = node.get_mut("properties")?.get_mut(&segment.name)?;
        if segment.array {
            node = node.get_mut("items")?;
        }
    }
    node.as_object_mut()
}

/// A by-reference argument that can't be expanded.
#[derive(Debug, thiserror::Error)]
pub enum ArgReferenceError {
    #[error("set either `{field}` or `{reference}`, not both")]
    Both { field: String, reference: String },
    #[error("`{reference}` must be a file name string")]
    NotAString { reference: String },
    #[error("`{reference}`: cannot read '{file}': {reason}")]
    Unreadable {
        reference: String,
        file: String,
        reason: String,
    },
}

/// A `<field>_file` key found in the model's arguments.
struct PendingReference {
    /// JSON pointer to the object holding the field.
    object_pointer: String,
    field: String,
    /// The reference's location in the arguments (`files[2].content_file`).
    display: String,
    /// Referenced file name.
    file: Option<String>,
}

fn join_display(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn escape_pointer(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// Walk `node` along `parents` (fanning out over `[]` arrays) and record every
/// `<field>_file` key found in the objects at the end. Absent or mistyped
/// intermediate values are left for the tool itself to reject.
fn collect_references(
    node: &Value,
    parents: &[FieldSegment],
    field: &str,
    pointer: String,
    display: String,
    out: &mut Vec<PendingReference>,
) -> Result<(), ArgReferenceError> {
    let Some((segment, rest)) = parents.split_first() else {
        let Some(object) = node.as_object() else {
            return Ok(());
        };
        let reference = reference_key(field);
        let reference_display = join_display(&display, &reference);
        let file = match object.get(&reference) {
            None => return Ok(()),
            // An explicit `null` reference is removed, not expanded.
            Some(Value::Null) => None,
            Some(Value::String(file)) => {
                if object.get(field).is_some_and(|value| !value.is_null()) {
                    return Err(ArgReferenceError::Both {
                        field: join_display(&display, field),
                        reference: reference_display,
                    });
                }
                Some(file.clone())
            }
            Some(_) => {
                return Err(ArgReferenceError::NotAString {
                    reference: reference_display,
                });
            }
        };
        out.push(PendingReference {
            object_pointer: pointer,
            field: field.to_string(),
            display: reference_display,
            file,
        });
        return Ok(());
    };

    let Some(child) = node.get(&segment.name) else {
        return Ok(());
    };
    let pointer = format!("{pointer}/{}", escape_pointer(&segment.name));
    let display = join_display(&display, &segment.name);
    if !segment.array {
        return collect_references(child, rest, field, pointer, display, out);
    }
    let Some(items) = child.as_array() else {
        return Ok(());
    };
    for (i, item) in items.iter().enumerate() {
        collect_references(
            item,
            rest,
            field,
            format!("{pointer}/{i}"),
            format!("{display}[{i}]"),
            out,
        )?;
    }
    Ok(())
}

/// Replace every `<field>_file` reference in `args` with the referenced
/// file's contents under `<field>`. Every reference is checked before any
/// file is read, so a malformed call fails without touching the disk.
pub async fn resolve_references(
    mut args: Value,
    fields: &[FieldPath],
    resolver: &ReferenceResolver,
) -> Result<Value, ArgReferenceError> {
    let mut pending = Vec::new();
    for path in fields {
        collect_references(
            &args,
            path.parents(),
            path.field(),
            String::new(),
            String::new(),
            &mut pending,
        )?;
    }

    for reference in pending {
        let content =
            match &reference.file {
                Some(file) => Some(resolver.read(file).await.map_err(|reason| {
                    ArgReferenceError::Unreadable {
                        reference: reference.display.clone(),
                        file: file.clone(),
                        reason,
                    }
                })?),
                None => None,
            };
        // Pointers came from these same args; only overlapping field paths
        // (one configured field nested inside another) could invalidate one.
        let Some(object) = args
            .pointer_mut(&reference.object_pointer)
            .and_then(Value::as_object_mut)
        else {
            tracing::debug!(
                "by_reference: '{}' no longer addressable",
                reference.display
            );
            continue;
        };
        object.remove(&reference_key(&reference.field));
        if let Some(content) = content {
            tracing::debug!(
                "by_reference: expanded {} ({} bytes)",
                reference.display,
                content.len()
            );
            object.insert(reference.field, Value::String(content));
        }
    }
    Ok(args)
}

/// Reads the files that references name.
#[derive(Clone)]
pub struct ReferenceResolver {
    storage: Arc<ScratchpadStorage>,
    persistence: Option<Arc<Mutex<ExecutionPersistence>>>,
}

impl ReferenceResolver {
    pub fn new(
        storage: Arc<ScratchpadStorage>,
        persistence: Option<Arc<Mutex<ExecutionPersistence>>>,
    ) -> Self {
        Self {
            storage,
            persistence,
        }
    }

    /// Read `file` as a scratchpad file token (the names the scratchpad read
    /// tools take, confined to the storage read root) or, failing that, as an
    /// artifact filename in the current orchestration run (the same
    /// resolution `read_artifact` uses). The error is a model-facing reason.
    pub(crate) async fn read(&self, file: &str) -> Result<String, String> {
        let path = self
            .storage
            .validate_path(file)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(content) = read_bounded(&path).await? {
            return Ok(content);
        }

        if let Some(persistence) = &self.persistence {
            let persistence = persistence.lock().await;
            // `artifact_path` rejects anything but a bare artifact filename.
            if persistence.is_enabled()
                && let Ok(path) = persistence.artifact_path(file)
                && let Some(content) = read_bounded(&path).await?
            {
                return Ok(content);
            }
        }

        Err("no scratchpad file or artifact by that name".to_string())
    }
}

/// Read `path` as UTF-8 text, refusing files over [`MAX_RAW_PAYLOAD_BYTES`]
/// before reading them; `Ok(None)` when there is no such file.
async fn read_bounded(path: &Path) -> Result<Option<String>, String> {
    let size = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(describe_read_error(&e)),
    };
    if size > MAX_RAW_PAYLOAD_BYTES as u64 {
        return Err(format!(
            "the file is {size} bytes; references are limited to {MAX_RAW_PAYLOAD_BYTES} bytes"
        ));
    }
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(describe_read_error(&e)),
    }
}

fn describe_read_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::InvalidData => "not UTF-8 text".to_string(),
        _ => e.to_string(),
    }
}

/// A tool whose configured string fields also accept a `<field>_file`
/// reference.
#[derive(Clone)]
pub struct ArgReferenceTool<T> {
    inner: T,
    fields: Arc<[FieldPath]>,
    resolver: ReferenceResolver,
    /// The configured fields the inner schema takes a reference for.
    applied: Arc<OnceCell<Arc<[FieldPath]>>>,
}

impl<T> ArgReferenceTool<T>
where
    T: RigTool<Args = Value, Output = String, Error = ToolError> + Clone + Send + Sync + 'static,
{
    pub fn new(inner: T, fields: Vec<FieldPath>, resolver: ReferenceResolver) -> Self {
        Self {
            inner,
            fields: fields.into(),
            resolver,
            applied: Arc::new(OnceCell::new()),
        }
    }

    /// Derived once from the inner definition, so the schema the model sees
    /// and the arguments expanded at call time always agree.
    async fn applied_fields(&self) -> Arc<[FieldPath]> {
        self.applied
            .get_or_init(|| async {
                let mut definition = self.inner.definition(String::new()).await;
                add_reference_fields(&mut definition.parameters, &self.fields).into()
            })
            .await
            .clone()
    }
}

impl<T> RigTool for ArgReferenceTool<T>
where
    T: RigTool<Args = Value, Output = String, Error = ToolError> + Clone + Send + Sync + 'static,
{
    type Error = ToolError;
    type Args = Value;
    type Output = String;

    /// Placeholder the trait requires: registration, events and schemas use
    /// [`name`](RigTool::name), which is the wrapped tool's name.
    const NAME: &'static str = "arg_reference_tool";

    fn name(&self) -> String {
        self.inner.name()
    }

    #[allow(refining_impl_trait)]
    fn definition(
        &self,
        prompt: String,
    ) -> Pin<Box<dyn Future<Output = ToolDefinition> + Send + Sync + '_>> {
        Box::pin(async move {
            let mut definition = self.inner.definition(prompt).await;
            let applied = add_reference_fields(&mut definition.parameters, &self.fields);
            let _ = self.applied.set(applied.into());
            definition
        })
    }

    #[allow(refining_impl_trait)]
    fn call(
        &self,
        args: Self::Args,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + '_>> {
        Box::pin(async move {
            let applied = self.applied_fields().await;
            let args = resolve_references(args, &applied, &self.resolver)
                .await
                .map_err(|e| ToolError::ToolCallError(Box::new(e)))?;
            self.inner.call(args).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;
    use aura_config::ServerScratchpadConfig;
    use serde_json::json;
    use tempfile::TempDir;

    fn paths(raw: &[&str]) -> Vec<FieldPath> {
        raw.iter().map(|p| FieldPath::parse(p).unwrap()).collect()
    }

    fn write_file_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string", "description": "File content." },
                "message": { "type": "string" }
            },
            "required": ["path", "content", "message"]
        })
    }

    fn push_files_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "content": { "type": "string" }
                        },
                        "required": ["path", "content"]
                    }
                }
            },
            "required": ["files"]
        })
    }

    async fn storage(tmp: &TempDir) -> Arc<ScratchpadStorage> {
        Arc::new(
            ScratchpadStorage::with_base_dir(tmp.path(), "req-ref")
                .await
                .unwrap(),
        )
    }

    async fn resolver_with_file(tmp: &TempDir, name: &str, content: &str) -> ReferenceResolver {
        let storage = storage(tmp).await;
        tokio::fs::write(storage.dir().join(name), content)
            .await
            .unwrap();
        ReferenceResolver::new(storage, None)
    }

    // --- by_reference_map ---

    fn server(by_reference: &[(&str, &[&str])]) -> McpServerConfig {
        McpServerConfig::HttpStreamable {
            url: "http://test".to_string(),
            headers: HashMap::new(),
            description: None,
            headers_from_request: HashMap::new(),
            scratchpad: ServerScratchpadConfig {
                by_reference: by_reference
                    .iter()
                    .map(|(pattern, fields)| (pattern.to_string(), paths(fields)))
                    .collect(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn by_reference_map_scopes_patterns_per_server_and_prefers_longest() {
        let mcp = McpConfig {
            servers: HashMap::from([
                (
                    "github".to_string(),
                    server(&[
                        ("*", &["body"]),
                        ("create_or_update_file", &["content"]),
                        ("push_*", &["files[].content"]),
                    ]),
                ),
                ("other".to_string(), server(&[])),
            ]),
            sanitize_schemas: false,
        };
        let tools = HashMap::from([
            (
                "github".to_string(),
                vec![
                    "create_or_update_file".to_string(),
                    "push_files".to_string(),
                    "create_issue".to_string(),
                ],
            ),
            ("other".to_string(), vec!["write_note".to_string()]),
        ]);

        let resolved = by_reference_map(Some(&mcp), &tools);

        let fields = |tool| resolved.get("github", tool).map(<[FieldPath]>::to_vec);
        assert_eq!(fields("create_or_update_file"), Some(paths(&["content"])));
        assert_eq!(fields("push_files"), Some(paths(&["files[].content"])));
        assert_eq!(fields("create_issue"), Some(paths(&["body"])));
        assert!(
            resolved.get("other", "write_note").is_none(),
            "github patterns must not apply to another server's tools"
        );
        assert_eq!(resolved.len(), 3);
    }

    /// A reference sends stored content to the tool's own server, so a
    /// same-named tool on a server that didn't opt in must not get one.
    #[test]
    fn by_reference_map_keeps_same_named_tools_on_other_servers_out() {
        let mcp = McpConfig {
            servers: HashMap::from([
                (
                    "github".to_string(),
                    server(&[("create_or_update_file", &["content"])]),
                ),
                ("mirror".to_string(), server(&[])),
            ]),
            sanitize_schemas: false,
        };
        let tools = HashMap::from([
            (
                "github".to_string(),
                vec!["create_or_update_file".to_string()],
            ),
            (
                "mirror".to_string(),
                vec!["create_or_update_file".to_string()],
            ),
        ]);

        let resolved = by_reference_map(Some(&mcp), &tools);

        assert!(resolved.get("github", "create_or_update_file").is_some());
        assert!(resolved.get("mirror", "create_or_update_file").is_none());
        assert!(resolved.contains_tool("create_or_update_file"));
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn reference_twin_tokens_counts_reachable_fields_only() {
        use crate::scratchpad::TiktokenCounter;

        let mcp = McpConfig {
            servers: HashMap::from([(
                "github".to_string(),
                server(&[
                    ("create_or_update_file", &["content"]),
                    ("push_files", &["files[].content"]),
                ]),
            )]),
            sanitize_schemas: false,
        };
        let tools = HashMap::from([(
            "github".to_string(),
            vec![
                "create_or_update_file".to_string(),
                "push_files".to_string(),
            ],
        )]);
        let by_reference = by_reference_map(Some(&mcp), &tools);
        let counter = TiktokenCounter::default_counter();

        let all = reference_twin_tokens(&counter, &by_reference, None);
        let one = reference_twin_tokens(&counter, &by_reference, Some(&["push_*".to_string()]));
        let none = reference_twin_tokens(&counter, &by_reference, Some(&[]));

        assert!(one > 0, "a reachable field adds tokens");
        assert_eq!(all, 2 * one, "each configured field costs the same text");
        assert_eq!(none, 0);

        // The estimate covers exactly what add_reference_fields inserts.
        let reference = reference_key("content");
        let mut schema = write_file_schema();
        let before = counter.count_tokens(&schema.to_string());
        add_reference_fields(&mut schema, &paths(&["content"]));
        let added = counter.count_tokens(&schema.to_string()) - before;
        assert!(
            added <= one + counter.count_tokens(&reference),
            "estimate {one} should cover the {added} tokens actually added"
        );
    }

    // --- add_reference_fields ---

    #[test]
    fn add_reference_fields_adds_the_file_twin_and_relaxes_required() {
        let mut schema = write_file_schema();
        let applied = add_reference_fields(&mut schema, &paths(&["content"]));

        assert_eq!(applied, paths(&["content"]));
        assert_eq!(schema["properties"]["content_file"]["type"], "string");
        assert_eq!(schema["required"], json!(["path", "message"]));
        let description = schema["properties"]["content"]["description"]
            .as_str()
            .unwrap();
        assert!(description.starts_with("File content."));
        assert!(description.contains("content_file"));
        assert!(
            schema["properties"].get("path_file").is_none(),
            "only configured fields get a twin"
        );
    }

    #[test]
    fn add_reference_fields_reaches_into_array_items() {
        let mut schema = push_files_schema();
        let applied = add_reference_fields(&mut schema, &paths(&["files[].content"]));

        assert_eq!(applied, paths(&["files[].content"]));
        let item = &schema["properties"]["files"]["items"];
        assert_eq!(item["properties"]["content_file"]["type"], "string");
        assert_eq!(item["required"], json!(["path"]));
        assert_eq!(schema["required"], json!(["files"]), "top level untouched");
    }

    #[test]
    fn add_reference_fields_skips_undeclared_and_colliding_fields() {
        let mut schema = write_file_schema();
        schema["properties"]["message_file"] = json!({ "type": "boolean" });
        let before = schema.clone();

        let applied = add_reference_fields(&mut schema, &paths(&["missing", "message", "a.b"]));

        assert!(applied.is_empty());
        assert_eq!(schema, before, "skipped fields leave the schema untouched");
    }

    // --- resolve_references ---

    #[tokio::test]
    async fn resolve_expands_a_reference_byte_for_byte() {
        let tmp = TempDir::new().unwrap();
        let file = "# Runbook\n\n- étape un\n- step two\n\n";
        let resolver = resolver_with_file(&tmp, "task_1-w-get-0-abc.raw.md", file).await;

        let args = json!({
            "path": "docs/runbook.md",
            "content_file": "task_1-w-get-0-abc.raw.md",
            "message": "restore"
        });
        let resolved = resolve_references(args, &paths(&["content"]), &resolver)
            .await
            .unwrap();

        assert_eq!(
            resolved,
            json!({ "path": "docs/runbook.md", "content": file, "message": "restore" })
        );
    }

    #[tokio::test]
    async fn resolve_expands_references_inside_arrays() {
        let tmp = TempDir::new().unwrap();
        let resolver = resolver_with_file(&tmp, "a.txt", "from file").await;

        let args = json!({ "files": [
            { "path": "x", "content": "inline" },
            { "path": "y", "content_file": "a.txt" }
        ]});
        let resolved = resolve_references(args, &paths(&["files[].content"]), &resolver)
            .await
            .unwrap();

        assert_eq!(
            resolved,
            json!({ "files": [
                { "path": "x", "content": "inline" },
                { "path": "y", "content": "from file" }
            ]})
        );
    }

    #[tokio::test]
    async fn resolve_leaves_inline_values_alone() {
        let tmp = TempDir::new().unwrap();
        let resolver = ReferenceResolver::new(storage(&tmp).await, None);
        let args = json!({ "content": "typed out" });
        let resolved = resolve_references(args.clone(), &paths(&["content"]), &resolver)
            .await
            .unwrap();
        assert_eq!(resolved, args);
    }

    #[tokio::test]
    async fn resolve_drops_a_null_reference() {
        let tmp = TempDir::new().unwrap();
        let resolver = ReferenceResolver::new(storage(&tmp).await, None);
        let args = json!({ "content": "typed out", "content_file": null });
        let resolved = resolve_references(args, &paths(&["content"]), &resolver)
            .await
            .unwrap();
        assert_eq!(resolved, json!({ "content": "typed out" }));
    }

    #[tokio::test]
    async fn resolve_rejects_both_the_field_and_its_reference() {
        let tmp = TempDir::new().unwrap();
        let resolver = resolver_with_file(&tmp, "a.txt", "x").await;
        let args = json!({ "files": [{ "content": "inline", "content_file": "a.txt" }] });

        let err = resolve_references(args, &paths(&["files[].content"]), &resolver)
            .await
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "set either `files[0].content` or `files[0].content_file`, not both"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_a_non_string_reference() {
        let tmp = TempDir::new().unwrap();
        let resolver = ReferenceResolver::new(storage(&tmp).await, None);
        let err = resolve_references(
            json!({ "content_file": 7 }),
            &paths(&["content"]),
            &resolver,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ArgReferenceError::NotAString { .. }), "{err}");
    }

    #[tokio::test]
    async fn resolve_rejects_a_missing_file() {
        let tmp = TempDir::new().unwrap();
        let resolver = ReferenceResolver::new(storage(&tmp).await, None);
        let err = resolve_references(
            json!({ "content_file": "nope.txt" }),
            &paths(&["content"]),
            &resolver,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "`content_file`: cannot read 'nope.txt': no scratchpad file or artifact by that name"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_a_path_outside_the_read_root() {
        let tmp = TempDir::new().unwrap();
        tokio::fs::write(tmp.path().join("secret.txt"), "top secret")
            .await
            .unwrap();
        let resolver = ReferenceResolver::new(storage(&tmp).await, None);

        let err = resolve_references(
            json!({ "content_file": "../secret.txt" }),
            &paths(&["content"]),
            &resolver,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("outside scratchpad directory"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_a_file_over_the_size_limit() {
        let tmp = TempDir::new().unwrap();
        let big = "x".repeat(MAX_RAW_PAYLOAD_BYTES + 1);
        let resolver = resolver_with_file(&tmp, "big.txt", &big).await;

        let err = resolve_references(
            json!({ "content_file": "big.txt" }),
            &paths(&["content"]),
            &resolver,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("references are limited to"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn resolve_falls_back_to_current_run_artifacts() {
        let tmp = TempDir::new().unwrap();
        let persistence = ExecutionPersistence::new(tmp.path().join("memory"), None)
            .await
            .unwrap();
        let artifact = "successfully downloaded text file (SHA: abc)\nbody\n";
        let filename = persistence
            .write_tool_output_artifact(0, "writer", 1, "get_file_contents", 0, artifact)
            .await
            .unwrap();
        let storage = Arc::new(
            ScratchpadStorage::in_dir(&persistence.run_path().join("iteration-1"))
                .await
                .unwrap()
                .with_read_root(persistence.run_path().to_path_buf()),
        );
        let resolver = ReferenceResolver::new(storage, Some(Arc::new(Mutex::new(persistence))));

        let resolved = resolve_references(
            json!({ "content_file": filename }),
            &paths(&["content"]),
            &resolver,
        )
        .await
        .unwrap();

        assert_eq!(resolved, json!({ "content": artifact }));
    }

    // --- ArgReferenceTool ---

    #[derive(Clone)]
    struct RecordingTool {
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl RigTool for RecordingTool {
        const NAME: &'static str = "create_or_update_file";
        type Error = ToolError;
        type Args = Value;
        type Output = String;

        async fn definition(&self, _prompt: String) -> ToolDefinition {
            ToolDefinition {
                name: Self::NAME.to_string(),
                description: String::new(),
                parameters: write_file_schema(),
            }
        }

        async fn call(&self, args: Value) -> Result<String, ToolError> {
            self.seen.lock().unwrap().push(args);
            Ok("written".to_string())
        }
    }

    async fn reference_tool(
        tmp: &TempDir,
    ) -> (
        ArgReferenceTool<RecordingTool>,
        Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let resolver = resolver_with_file(tmp, "file.raw.md", "exact bytes\n").await;
        let tool = ArgReferenceTool::new(
            RecordingTool { seen: seen.clone() },
            paths(&["content", "not_in_schema"]),
            resolver,
        );
        (tool, seen)
    }

    #[tokio::test]
    async fn tool_advertises_the_twin_and_sends_the_expanded_value() {
        let tmp = TempDir::new().unwrap();
        let (tool, seen) = reference_tool(&tmp).await;

        let definition = tool.definition(String::new()).await;
        assert_eq!(definition.name, "create_or_update_file");
        assert!(definition.parameters["properties"]["content_file"].is_object());

        let output = tool
            .call(json!({ "path": "p", "message": "m", "content_file": "file.raw.md" }))
            .await
            .unwrap();

        assert_eq!(output, "written");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [json!({ "path": "p", "message": "m", "content": "exact bytes\n" })]
        );
    }

    #[tokio::test]
    async fn tool_resolves_even_before_its_definition_is_requested() {
        let tmp = TempDir::new().unwrap();
        let (tool, seen) = reference_tool(&tmp).await;

        tool.call(json!({ "content_file": "file.raw.md" }))
            .await
            .unwrap();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [json!({ "content": "exact bytes\n" })]
        );
    }

    #[tokio::test]
    async fn tool_surfaces_a_bad_reference_as_a_tool_error_without_calling_through() {
        let tmp = TempDir::new().unwrap();
        let (tool, seen) = reference_tool(&tmp).await;

        let err = tool
            .call(json!({ "content_file": "missing.md" }))
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("cannot read 'missing.md'"),
            "{err}"
        );
        assert!(seen.lock().unwrap().is_empty(), "inner tool must not run");
    }

    /// Wrappers see the model's arguments (the reference), not the expanded
    /// file: persistence, observer events and the HITL gate stay small.
    #[tokio::test]
    async fn wrappers_around_the_tool_see_the_reference_not_the_content() {
        use crate::mcp::CallOutcome;
        use crate::tool_wrapper::{
            ToolCallContext, ToolWrapper, TransformOutputResult, WrappedTool,
        };

        struct RecordArgs(Arc<std::sync::Mutex<Option<Value>>>);

        #[async_trait::async_trait]
        impl ToolWrapper for RecordArgs {
            async fn transform_output(
                &self,
                output: String,
                _outcome: &CallOutcome,
                ctx: &ToolCallContext,
                _extracted: Option<&Value>,
            ) -> TransformOutputResult {
                *self.0.lock().unwrap() = ctx.metadata.clone();
                TransformOutputResult::new(output)
            }
        }

        let tmp = TempDir::new().unwrap();
        let (tool, seen) = reference_tool(&tmp).await;
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let wrapped = WrappedTool::new(tool, Arc::new(RecordArgs(recorded.clone())));

        let args = json!({ "path": "p", "content_file": "file.raw.md" });
        wrapped.call(args.clone()).await.unwrap();

        assert_eq!(recorded.lock().unwrap().clone(), Some(args));
        assert_eq!(seen.lock().unwrap()[0]["content"], "exact bytes\n");
    }

    /// Wrapped *around* the wrapper chain (the HITL-gated placement), a gate's
    /// `pre_call` sees the bytes that will be sent, not the file name.
    #[tokio::test]
    async fn outside_the_wrapper_chain_a_gate_sees_the_expanded_content() {
        use crate::tool_wrapper::{PreCallOutcome, ToolCallContext, ToolWrapper, WrappedTool};

        struct RecordPreCall(Arc<std::sync::Mutex<Option<Value>>>);

        #[async_trait::async_trait]
        impl ToolWrapper for RecordPreCall {
            async fn pre_call(
                &self,
                args: &Value,
                _ctx: &ToolCallContext,
            ) -> Result<PreCallOutcome, ToolError> {
                *self.0.lock().unwrap() = Some(args.clone());
                Ok(PreCallOutcome::Proceed { overrides: None })
            }
        }

        let tmp = TempDir::new().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gate_saw = Arc::new(std::sync::Mutex::new(None));
        let inner = WrappedTool::new(
            RecordingTool { seen: seen.clone() },
            Arc::new(RecordPreCall(gate_saw.clone())),
        );
        let resolver = resolver_with_file(&tmp, "file.raw.md", "exact bytes\n").await;
        let tool = ArgReferenceTool::new(inner, paths(&["content"]), resolver);

        let definition = tool.definition(String::new()).await;
        assert!(definition.parameters["properties"]["content_file"].is_object());

        tool.call(json!({ "path": "p", "content_file": "file.raw.md" }))
            .await
            .unwrap();

        let expanded = json!({ "path": "p", "content": "exact bytes\n" });
        assert_eq!(gate_saw.lock().unwrap().clone(), Some(expanded.clone()));
        assert_eq!(seen.lock().unwrap().as_slice(), [expanded]);
    }
}
