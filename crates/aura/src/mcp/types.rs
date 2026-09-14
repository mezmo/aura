//! [`AuraTool`]: an MCP tool discovered from a server, paired with the
//! namespace (server config key) that exposes it.
use aura_events::{McpToolAnnotations, McpToolOverview};
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::sync::Arc;

const NAMESPACE_DELIMETER: char = ':';

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolNamespace(String);
impl ToolNamespace {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, pattern: &str) -> bool {
        match pattern.split_once(NAMESPACE_DELIMETER) {
            // In order to not break existing tool patterns, treat the lack
            // of namespace as matching every namespace value.
            None => true,
            Some((ns_pattern, _)) => crate::config::glob_match(ns_pattern, &self.0),
        }
    }
}

impl From<String> for ToolNamespace {
    fn from(value: String) -> Self {
        ToolNamespace(value)
    }
}

impl From<&str> for ToolNamespace {
    fn from(value: &str) -> Self {
        ToolNamespace(value.to_owned())
    }
}

impl Display for ToolNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolName(String);
impl ToolName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, pattern: &str) -> bool {
        let tool_pattern = match pattern.split_once(NAMESPACE_DELIMETER) {
            // split_once returns None if the delimeter is not in the string, which
            // is our key for filter without namesapce (backward compat situation)
            None => pattern,
            Some((_, tool_name)) => tool_name,
        };
        crate::config::glob_match(tool_pattern, &self.0)
    }
}

impl From<String> for ToolName {
    fn from(value: String) -> Self {
        ToolName(value)
    }
}

impl From<&str> for ToolName {
    fn from(value: &str) -> Self {
        ToolName(value.to_owned())
    }
}

impl From<&rmcp::model::Tool> for ToolName {
    fn from(value: &rmcp::model::Tool) -> Self {
        ToolName(value.name.to_string())
    }
}

impl Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

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
        let name = ToolName::from(&inner);
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

    pub fn title(&self) -> Option<&String> {
        self.inner.title.as_ref()
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

    /// Checks a single `mcp_filter` pattern against this tool's
    /// namespace and name.
    pub fn matches(&self, pattern: &str) -> bool {
        self.namespace.matches(pattern) && self.name.matches(pattern)
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
            name: ToolName(name),
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
            name,
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

    mod tool_namespace {
        use super::*;

        #[test]
        fn tool_namespace_string_repr() {
            let ns = ToolNamespace::from("k8s");
            assert_eq!(ns.as_str(), "k8s");
            assert_eq!(ns.to_string(), "k8s");
            assert_eq!(format!("{ns}"), "k8s");
            assert_eq!(serde_json::to_string(&ns).expect("json value"), "\"k8s\"");
        }

        #[test]
        fn match_pattern_with_exact_namespace_component() {
            let ns = ToolNamespace::from("mezmo");
            assert!(ns.matches("mezmo:tool"));
            assert!(!ns.matches("logdna:tool"));
        }

        #[test]
        fn match_pattern_with_glob_namespace_component() {
            let ns = ToolNamespace::from("mezmo");
            assert!(ns.matches("*ezmo:tool"));
            assert!(ns.matches("mez*:tool"));
            assert!(!ns.matches("*abc:tool"));
            assert!(!ns.matches("abc*:tool"));
            assert!(!ns.matches("*abc*:tool"));
        }

        #[test]
        fn match_patten_without_namespace_component() {
            let ns = ToolNamespace::from("git");
            assert!(ns.matches("tool"));
            assert!(ns.matches("*tool"));
            assert!(ns.matches("tool*"));
            assert!(ns.matches("*tool*"));
        }
    }

    mod tool_name {
        use super::*;

        #[test]
        fn tool_name_string_repr() {
            let tool = ToolName::from("list_pods");
            assert_eq!(tool.as_str(), "list_pods");
            assert_eq!(tool.to_string(), "list_pods");
            assert_eq!(format!("{tool}"), "list_pods");
            assert_eq!(
                serde_json::to_string(&tool).expect("json value"),
                "\"list_pods\""
            );
        }

        #[test]
        fn match_pattern_with_exact_tool_name() {
            let tool = ToolName::from("get_pod_details");
            assert!(tool.matches("get_pod_details"));
            assert!(tool.matches("k8s:get_pod_details"));
            assert!(tool.matches("k8s*:get_pod_details"));
            assert!(tool.matches("*k8s:get_pod_details"));
            assert!(!tool.matches("pod_details"));
        }

        #[test]
        fn match_pattern_with_glob_tool_name() {
            let tool = ToolName::from("list_files");
            // bare tool name globs
            assert!(tool.matches("list*"));
            assert!(tool.matches("*files"));
            assert!(tool.matches("li*les"));

            // tool name globs with static namespace part
            assert!(tool.matches("fs:list*"));
            assert!(tool.matches("fs:*files"));

            // tool name globs with globbed namespace part
            assert!(tool.matches("fs*:list*"));
            assert!(tool.matches("fs*:*files"));
            assert!(tool.matches("*fs:list*"));
            assert!(tool.matches("*fs:*files"));

            // tool name doesn't match
            assert!(!tool.matches("delete_file"));
            assert!(!tool.matches("fs:delete_file"));
        }
    }

    mod aura_tool {
        use super::*;

        fn rmcp_tool(name: &str) -> rmcp::model::Tool {
            rmcp::model::Tool::new(
                name.to_owned(),
                "a test tool".to_owned(),
                std::sync::Arc::new(serde_json::Map::new()),
            )
        }

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
        fn matches_bare_pattern_across_any_namespace() {
            let a = AuraTool::new(rmcp_tool("get_logs"), "mezmo");
            let b = AuraTool::new(rmcp_tool("get_logs"), "k8s");
            assert!(a.matches("get_logs"));
            assert!(b.matches("get_logs"));
        }

        #[test]
        fn matches_namespace_scoped_pattern() {
            let mezmo = AuraTool::new(rmcp_tool("get_logs"), "mezmo");
            let k8s = AuraTool::new(rmcp_tool("get_logs"), "k8s");
            assert!(mezmo.matches("mezmo:get_logs"));
            assert!(!k8s.matches("mezmo:get_logs"));
        }

        #[test]
        fn description_and_schema_pass_through_from_inner_tool() {
            let tool = AuraTool::new(rmcp_tool("pipeline"), "mezmo");
            assert_eq!(tool.description(), Some("a test tool"));
            assert_eq!(tool.input_schema(), serde_json::json!({}));
        }
    }
}
