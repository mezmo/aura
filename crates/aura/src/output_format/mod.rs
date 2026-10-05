//! Per-tool output formatters, configured via
//! `[mcp.servers.<name>.output_format]`.
//!
//! [`OutputFormatWrapper`] rewrites a recognised tool output into a compact
//! text form before scratchpad interception sees it. Output a formatter does
//! not recognise, tool errors, and calls made with `_aura_raw: true` pass
//! through unchanged.
//!
//! Formatters:
//! - `prometheus`: Prometheus HTTP API query results ([`prometheus`]).

mod prometheus;

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::config::McpConfig;
use crate::mcp::CallOutcome;
use crate::orchestration::persistence_wrapper::strip_artifact_footer;
use crate::tool_wrapper::{
    ToolCallContext, ToolWrapper, TransformArgsResult, TransformOutputResult,
};
use aura_config::{OutputFormatEntry, glob_match};

/// Tool argument the model sets to receive the unformatted output.
pub const RAW_FIELD: &str = "_aura_raw";

/// Key of this wrapper's entry in the `extracted` payload.
const EXTRACTED_KEY: &str = "_aura_output_format";

/// Applies the configured output formatter to each MCP tool's output.
///
/// Compose it so its `transform_output` runs before `ScratchpadWrapper`'s:
/// formatted output that still reaches a tool's `min_tokens` is then
/// intercepted as usual.
pub struct OutputFormatWrapper {
    /// Server name → `(pattern, entry)`, longest pattern first.
    servers: HashMap<String, Vec<(String, OutputFormatEntry)>>,
}

impl OutputFormatWrapper {
    /// Build from the MCP config. `None` when no server configures a
    /// formatter, so callers add nothing to the wrapper chain.
    pub fn from_mcp_config(mcp: Option<&McpConfig>) -> Option<Self> {
        let servers: HashMap<_, _> = mcp?
            .servers
            .iter()
            .filter_map(|(name, server)| {
                let mut entries: Vec<_> = server
                    .output_format()
                    .iter()
                    .map(|(pattern, entry)| (pattern.clone(), entry.clone()))
                    .collect();
                if entries.is_empty() {
                    return None;
                }
                entries.sort_by(|(a, _), (b, _)| b.len().cmp(&a.len()).then(a.cmp(b)));
                Some((name.clone(), entries))
            })
            .collect();
        (!servers.is_empty()).then_some(Self { servers })
    }

    /// The entry for this tool: the longest pattern on its server that
    /// matches its name. Tools without an MCP namespace never match.
    fn entry(&self, ctx: &ToolCallContext) -> Option<&OutputFormatEntry> {
        self.servers
            .get(ctx.tool_namespace.as_deref()?)?
            .iter()
            .find(|(pattern, _)| glob_match(pattern, &ctx.tool_name))
            .map(|(_, entry)| entry)
    }
}

/// Remove `_aura_raw` from the arguments; true when it was set.
fn take_raw_flag(mut args: Value) -> (bool, Value) {
    let raw = match args.as_object_mut().and_then(|o| o.remove(RAW_FIELD)) {
        Some(Value::Bool(b)) => b,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    };
    (raw, args)
}

/// This wrapper's entry in `extracted`, which is either its own object or,
/// under `ComposedWrapper`, an array of every wrapper's object.
fn own_extracted(extracted: Option<&Value>) -> Option<&Value> {
    let extracted = extracted?;
    match extracted.as_array() {
        Some(items) => items.iter().find_map(|item| item.get(EXTRACTED_KEY)),
        None => extracted.get(EXTRACTED_KEY),
    }
}

#[async_trait]
impl ToolWrapper for OutputFormatWrapper {
    fn wrap_schema_for(&self, mut schema: Value, ctx: &ToolCallContext) -> Value {
        if let Some(entry) = self.entry(ctx)
            && let Value::Object(obj) = &mut schema
            && let Value::Object(props) = obj
                .entry("properties")
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
        {
            props.insert(
                RAW_FIELD.to_string(),
                json!({
                    "type": "boolean",
                    "description": format!(
                        "Optional. Set true to receive this tool's unformatted response. \
                         By default the response is condensed by the {} formatter.",
                        entry.name()
                    ),
                }),
            );
        }
        schema
    }

    fn transform_args(&self, args: Value, ctx: &ToolCallContext) -> TransformArgsResult {
        let Some(entry) = self.entry(ctx) else {
            return TransformArgsResult::new(args);
        };
        let (raw, args) = take_raw_flag(args);
        let requested = match entry {
            OutputFormatEntry::Prometheus(opts) => {
                prometheus::RequestedRange::from_args(&args, opts)
            }
        };
        TransformArgsResult::with_extracted(
            args,
            json!({ EXTRACTED_KEY: { "raw": raw, "requested": requested } }),
        )
    }

    async fn transform_output(
        &self,
        output: String,
        outcome: &CallOutcome,
        ctx: &ToolCallContext,
        extracted: Option<&Value>,
    ) -> TransformOutputResult {
        if outcome.is_error() {
            return TransformOutputResult::new(output);
        }
        let Some(entry) = self.entry(ctx) else {
            return TransformOutputResult::new(output);
        };
        let state = own_extracted(extracted);
        if state
            .and_then(|s| s.get("raw"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return TransformOutputResult::new(output);
        }

        // A wrapper that runs earlier may have appended an artifact footer;
        // format the tool output alone and keep the footer.
        let content = strip_artifact_footer(&output);
        let footer = &output[content.len()..];

        let formatted = match entry {
            OutputFormatEntry::Prometheus(opts) => {
                let requested = state
                    .and_then(|s| s.get("requested"))
                    .and_then(|r| serde_json::from_value(r.clone()).ok())
                    .unwrap_or_default();
                prometheus::format(content, opts, &requested)
            }
        };

        match formatted {
            Some(text) => {
                tracing::info!(
                    tool = %ctx.tool_name,
                    formatter = entry.name(),
                    input_bytes = content.len(),
                    output_bytes = text.len(),
                    "output_format: formatted tool output"
                );
                TransformOutputResult::new(format!("{}{footer}", text.trim_end()))
            }
            None => {
                tracing::debug!(
                    tool = %ctx.tool_name,
                    formatter = entry.name(),
                    "output_format: output not recognised; passing through"
                );
                TransformOutputResult::new(output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_config::{McpServerConfig, PrometheusFormatOptions};

    const RANGE: &str = r#"{"resultType":"matrix","result":[{"metric":{"instance":"a"},"values":[[1791184800,"1"],[1791184830,"1"]]}]}"#;

    fn server(output_format: HashMap<String, OutputFormatEntry>) -> McpServerConfig {
        McpServerConfig::HttpStreamable {
            url: "http://localhost/mcp".into(),
            headers: HashMap::new(),
            description: None,
            headers_from_request: HashMap::new(),
            scratchpad: HashMap::new(),
            output_format,
            user_agent: None,
        }
    }

    fn prometheus(opts: PrometheusFormatOptions) -> OutputFormatEntry {
        OutputFormatEntry::Prometheus(opts)
    }

    fn wrapper() -> OutputFormatWrapper {
        let mcp = McpConfig {
            servers: HashMap::from([
                (
                    "metrics".to_string(),
                    server(HashMap::from([(
                        "*query*".to_string(),
                        prometheus(PrometheusFormatOptions {
                            step_arg: Some("step".into()),
                            ..Default::default()
                        }),
                    )])),
                ),
                ("other".to_string(), server(HashMap::new())),
            ]),
            ..Default::default()
        };
        OutputFormatWrapper::from_mcp_config(Some(&mcp)).expect("a formatter is configured")
    }

    fn ctx(namespace: Option<&str>, tool: &str) -> ToolCallContext {
        let mut ctx = ToolCallContext::new(tool);
        ctx.tool_namespace = namespace.map(String::from);
        ctx
    }

    async fn run(
        w: &OutputFormatWrapper,
        ctx: &ToolCallContext,
        args: Value,
        output: &str,
    ) -> String {
        let t = w.transform_args(args, ctx);
        w.transform_output(
            output.to_string(),
            &CallOutcome::Success(output.to_string()),
            ctx,
            t.extracted.as_ref(),
        )
        .await
        .output
    }

    #[test]
    fn no_wrapper_without_configured_formatters() {
        let mcp = McpConfig {
            servers: HashMap::from([("other".to_string(), server(HashMap::new()))]),
            ..Default::default()
        };
        assert!(OutputFormatWrapper::from_mcp_config(Some(&mcp)).is_none());
        assert!(OutputFormatWrapper::from_mcp_config(None).is_none());
    }

    #[test]
    fn longest_pattern_wins() {
        let mcp = McpConfig {
            servers: HashMap::from([(
                "metrics".to_string(),
                server(HashMap::from([
                    (
                        "*".to_string(),
                        prometheus(PrometheusFormatOptions::default()),
                    ),
                    (
                        "execute_range_query".to_string(),
                        prometheus(PrometheusFormatOptions {
                            max_series: 3,
                            ..Default::default()
                        }),
                    ),
                ])),
            )]),
            ..Default::default()
        };
        let w = OutputFormatWrapper::from_mcp_config(Some(&mcp)).unwrap();
        let OutputFormatEntry::Prometheus(opts) = w
            .entry(&ctx(Some("metrics"), "execute_range_query"))
            .unwrap();
        assert_eq!(opts.max_series, 3);
    }

    #[test]
    fn raw_field_is_added_only_to_formatted_tools() {
        let w = wrapper();
        let schema = json!({"type": "object", "properties": {"query": {"type": "string"}}});

        let formatted = w.wrap_schema_for(schema.clone(), &ctx(Some("metrics"), "execute_query"));
        assert_eq!(formatted["properties"][RAW_FIELD]["type"], "boolean");

        for other in [
            ctx(Some("metrics"), "list_metrics"),
            ctx(Some("other"), "execute_query"),
            ctx(None, "execute_query"),
        ] {
            assert_eq!(w.wrap_schema_for(schema.clone(), &other), schema);
        }
    }

    #[tokio::test]
    async fn formats_matching_tool_output() {
        let w = wrapper();
        let out = run(
            &w,
            &ctx(Some("metrics"), "execute_range_query"),
            json!({"query": "up", "step": "30s"}),
            RANGE,
        )
        .await;
        assert!(
            out.starts_with("Prometheus matrix result: 1 series."),
            "{out}"
        );
        assert!(
            out.contains("resolution: 30s (requested step 30s)"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn raw_flag_is_stripped_and_returns_the_original_output() {
        let w = wrapper();
        let c = ctx(Some("metrics"), "execute_range_query");
        let t = w.transform_args(json!({"query": "up", RAW_FIELD: true}), &c);
        assert_eq!(t.args, json!({"query": "up"}));
        let out = w
            .transform_output(
                RANGE.to_string(),
                &CallOutcome::Success(RANGE.to_string()),
                &c,
                Some(&Value::Array(vec![t.extracted.unwrap()])),
            )
            .await
            .output;
        assert_eq!(out, RANGE);
    }

    #[tokio::test]
    async fn unrecognised_output_and_other_tools_pass_through() {
        let w = wrapper();
        let text = "not a prometheus result";
        assert_eq!(
            run(&w, &ctx(Some("metrics"), "execute_query"), json!({}), text).await,
            text
        );
        assert_eq!(
            run(&w, &ctx(Some("other"), "execute_query"), json!({}), RANGE).await,
            RANGE
        );
    }

    #[tokio::test]
    async fn tool_errors_pass_through() {
        let w = wrapper();
        let c = ctx(Some("metrics"), "execute_query");
        let out = w
            .transform_output(
                RANGE.to_string(),
                &CallOutcome::GeneralToolError {
                    content: RANGE.to_string(),
                    code: None,
                },
                &c,
                None,
            )
            .await
            .output;
        assert_eq!(out, RANGE);
    }

    #[tokio::test]
    async fn artifact_footer_is_kept() {
        let w = wrapper();
        let footer = "\n\n[Tool output saved to artifact: result.json]";
        let out = run(
            &w,
            &ctx(Some("metrics"), "execute_range_query"),
            json!({}),
            &format!("{RANGE}{footer}"),
        )
        .await;
        assert!(out.starts_with("Prometheus matrix result"), "{out}");
        assert!(out.ends_with(footer), "{out}");
    }
}
