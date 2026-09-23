//! [`AuraTool`]: an MCP tool discovered from a server, paired with the
//! namespace (server config key) that exposes it.
use aura_config::GlobPattern;
use aura_events::{McpToolAnnotations, McpToolOverview};
pub use aura_events::{ToolName, ToolNamespace};
use std::sync::Arc;

/// An MCP tool discovered from a server, carrying its namespace alongside
/// the raw `rmcp` tool definition.
#[derive(Debug, Clone)]
pub struct AuraTool {
    /// The underlying tool as discovered from the MCP server (name already
    /// sanitized for LLM compatibility; schema optionally sanitized).
    inner: rmcp::model::Tool,
    namespace: ToolNamespace,
    name: ToolName,
}

impl AuraTool {
    /// `namespace` is the `[mcp.servers.<key>]` config key; `inner.name` is
    /// the already-sanitized bare tool name.
    pub fn new(inner: rmcp::model::Tool, namespace: impl Into<ToolNamespace>) -> Self {
        let name = ToolName::new(inner.name.as_ref());
        Self {
            namespace: namespace.into(),
            name,
            inner,
        }
    }

    pub fn name(&self) -> &ToolName {
        &self.name
    }

    pub fn namespace(&self) -> &ToolNamespace {
        &self.namespace
    }

    pub fn description(&self) -> Option<&str> {
        self.inner.description.as_deref()
    }

    pub fn input_schema(&self) -> serde_json::Value {
        self.inner.schema_as_json_value()
    }

    pub fn raw(&self) -> &rmcp::model::Tool {
        &self.inner
    }

    pub fn is_match(&self, glob: &GlobPattern) -> bool {
        glob.matches(Some(self.namespace().as_str()), self.name().as_str())
    }
}

/// Project a discovered MCP tool into its wire form.
///
/// These are the values AURA holds, not the ones the server advertised:
/// `McpManager::sanitize_mcp_tool` has already rewritten the name to the
/// LLM-safe character set and, under `[mcp].sanitize_schemas`, rewritten the
/// input schema. `name` is the tool's bare name — the only one ever sent to
/// a model, since hosted providers reject anything but alphanumerics and
/// `_` in tool names. Publishing that form is what makes the output usable
/// for governance — it names the tools AURA actually invokes with the
/// schemas the model actually receives.
///
/// `icons` is dropped; everything else in the MCP `Tool` object carries over.
#[allow(clippy::from_over_into)]
impl Into<McpToolOverview> for AuraTool {
    fn into(self) -> McpToolOverview {
        let AuraTool {
            name,
            inner:
                rmcp::model::Tool {
                    title,
                    description,
                    input_schema,
                    output_schema,
                    annotations,
                    meta,
                    ..
                },
            ..
        } = self;
        let input_schema = serde_json::Value::Object(Arc::unwrap_or_clone(input_schema));
        let output_schema =
            output_schema.map(|m| serde_json::Value::Object(Arc::unwrap_or_clone(m)));
        McpToolOverview {
            name: name.into_string(),
            title,
            description: description.map(std::borrow::Cow::into_owned),
            input_schema: Some(input_schema),
            output_schema,
            annotations: annotations.as_ref().map(|annotations| McpToolAnnotations {
                title: annotations.title.clone(),
                read_only_hint: annotations.read_only_hint,
                destructive_hint: annotations.destructive_hint,
                idempotent_hint: annotations.idempotent_hint,
                open_world_hint: annotations.open_world_hint,
            }),
            meta: meta.map(|m| serde_json::Value::Object(m.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rmcp_tool(name: &str) -> rmcp::model::Tool {
        rmcp::model::Tool::new(
            name.to_owned(),
            "a test tool".to_owned(),
            std::sync::Arc::new(serde_json::Map::new()),
        )
    }

    fn fully_populated_rmcp_tool() -> rmcp::model::Tool {
        rmcp::model::Tool {
            name: "get_logs".into(),
            title: Some("Get Logs".to_owned()),
            description: Some("fetch logs".into()),
            input_schema: Arc::new(
                serde_json::json!({"type": "object"})
                    .as_object()
                    .expect("object")
                    .clone(),
            ),
            output_schema: Some(Arc::new(
                serde_json::json!({"type": "string"})
                    .as_object()
                    .expect("object")
                    .clone(),
            )),
            annotations: Some(
                rmcp::model::ToolAnnotations::with_title("Get Logs")
                    .read_only(true)
                    .destructive(false)
                    .idempotent(true)
                    .open_world(false),
            ),
            icons: Some(vec![rmcp::model::Icon {
                src: "https://example.invalid/icon.png".to_owned(),
                mime_type: Some("image/png".to_owned()),
                sizes: Some(vec!["48x48".to_owned()]),
            }]),
            meta: Some(rmcp::model::Meta(
                serde_json::json!({"audit": "on"})
                    .as_object()
                    .expect("object")
                    .clone(),
            )),
        }
    }

    mod aura_tool {
        use super::*;

        #[test]
        fn name_and_namespace_are_tracked_separately() {
            // The model only ever sees the bare tool name; namespace is
            // metadata carried alongside it for filtering, HITL, governance,
            // and tracing — never concatenated into the model-facing name.
            let tool = AuraTool::new(rmcp_tool("pipeline"), "mezmo");
            assert_eq!(tool.name().to_string(), "pipeline");
            assert_eq!(tool.namespace().to_string(), "mezmo");
        }

        #[test]
        fn name_is_read_from_either_cow_variant_of_the_rmcp_name() {
            // `Tool::name` is a `Cow`, and `AuraTool::new` borrows it rather
            // than consuming the tool.
            let borrowed = AuraTool::new(rmcp_tool("list_pods"), "k8s");
            assert_eq!(borrowed.name().as_str(), "list_pods");

            let owned = AuraTool::new(rmcp_tool(&String::from("list_pods")), "k8s");
            assert_eq!(owned.name().as_str(), "list_pods");
        }

        #[test]
        fn matches_bare_pattern_across_any_namespace() {
            let glob = "get_logs".into();
            let a = AuraTool::new(rmcp_tool("get_logs"), "mezmo");
            let b = AuraTool::new(rmcp_tool("get_logs"), "k8s");
            assert!(a.is_match(&glob));
            assert!(b.is_match(&glob));
        }

        #[test]
        fn matches_namespace_scoped_pattern() {
            let glob = "mezmo:get_logs".into();
            let mezmo = AuraTool::new(rmcp_tool("get_logs"), "mezmo");
            let k8s = AuraTool::new(rmcp_tool("get_logs"), "k8s");
            assert!(mezmo.is_match(&glob));
            assert!(!k8s.is_match(&glob));
        }

        #[test]
        fn description_and_schema_pass_through_from_inner_tool() {
            let tool = AuraTool::new(rmcp_tool("pipeline"), "mezmo");
            assert_eq!(tool.description(), Some("a test tool"));
            assert_eq!(tool.input_schema(), serde_json::json!({}));
        }

        #[test]
        fn raw_and_input_schema_expose_the_inner_tool() {
            let inner = fully_populated_rmcp_tool();
            let tool = AuraTool::new(inner.clone(), "mezmo");
            assert_eq!(tool.raw(), &inner);
            assert_eq!(tool.input_schema(), serde_json::json!({"type": "object"}));
        }

        #[test]
        fn description_is_none_when_the_server_omits_it() {
            let mut inner = rmcp_tool("pipeline");
            inner.description = None;
            let tool = AuraTool::new(inner, "mezmo");
            assert_eq!(tool.description(), None);
        }

        #[test]
        fn matches_wildcard_patterns_in_either_position() {
            // `is_match` always hands the namespace over as `Some`, so a scoped pattern
            // can match — `GlobPattern::matches` rejects scoped patterns given `None`.
            let tool = AuraTool::new(rmcp_tool("get_logs"), "mezmo");
            assert!(tool.is_match(&"mezmo:*".into()));
            assert!(tool.is_match(&"*:get_logs".into()));
            assert!(tool.is_match(&"get_*".into()));
            assert!(!tool.is_match(&"k8s:*".into()));
        }

        #[test]
        fn into_overview_carries_every_field_across() {
            let overview: McpToolOverview =
                AuraTool::new(fully_populated_rmcp_tool(), "mezmo").into();
            assert_eq!(overview.name, "get_logs");
            assert_eq!(overview.title.as_deref(), Some("Get Logs"));
            assert_eq!(overview.description.as_deref(), Some("fetch logs"));
            assert_eq!(
                overview.input_schema,
                Some(serde_json::json!({"type": "object"}))
            );
            assert_eq!(
                overview.output_schema,
                Some(serde_json::json!({"type": "string"}))
            );
            assert_eq!(overview.meta, Some(serde_json::json!({"audit": "on"})));
            assert_eq!(
                overview.annotations,
                Some(McpToolAnnotations {
                    title: Some("Get Logs".to_owned()),
                    read_only_hint: Some(true),
                    destructive_hint: Some(false),
                    idempotent_hint: Some(true),
                    open_world_hint: Some(false),
                })
            );
        }

        #[test]
        fn into_overview_drops_icons() {
            // `McpToolOverview` has no counterpart field, so icons must not reach the wire.
            let overview: McpToolOverview =
                AuraTool::new(fully_populated_rmcp_tool(), "mezmo").into();
            let json = serde_json::to_value(&overview).expect("json value");
            assert!(json.get("icons").is_none());
        }

        #[test]
        fn into_overview_publishes_the_bare_name() {
            // Hosted providers reject anything but alphanumerics and `_` in tool names,
            // so the namespace is never folded into the published name.
            let overview: McpToolOverview = AuraTool::new(rmcp_tool("get_logs"), "mezmo").into();
            assert_eq!(overview.name, "get_logs");
        }

        #[test]
        fn into_overview_leaves_absent_fields_unset_but_always_sets_input_schema() {
            let overview: McpToolOverview = AuraTool::new(rmcp_tool("pipeline"), "mezmo").into();
            assert_eq!(overview.title, None);
            assert_eq!(overview.output_schema, None);
            assert_eq!(overview.annotations, None);
            assert_eq!(overview.meta, None);
            // `input_schema` is `Option` only so the type can carry a reduced summary
            // projection; this path always populates it, even when the schema is empty.
            assert_eq!(overview.input_schema, Some(serde_json::json!({})));
        }
    }
}
