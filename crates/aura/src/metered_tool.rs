//! The tool-execution metric, recorded around every tool an agent calls.
//!
//! An agent's tools all enter rig through `BuilderState` in `provider_agent`,
//! whatever composed the agent: a single-agent request, an orchestration
//! worker, or the coordinator. Wrapping each one there is what makes
//! `gen_ai.execute_tool.duration` independent of which stream the agent runs
//! on and of whether a middleware stack sits in front of the tool.

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;

use crate::streaming_request_hook::StreamingRequestHook;
use crate::tool_error_detection::is_reported_error;

/// A tool whose every call is timed into `gen_ai.execute_tool.duration`.
pub(crate) struct MeteredTool(Box<dyn ToolDyn>);

impl MeteredTool {
    pub(crate) fn new(tool: Box<dyn ToolDyn>) -> Self {
        Self(tool)
    }
}

impl ToolDyn for MeteredTool {
    fn name(&self) -> String {
        self.0.name()
    }

    fn definition<'a>(&'a self, prompt: String) -> WasmBoxedFuture<'a, ToolDefinition> {
        self.0.definition(prompt)
    }

    /// The tool as the agent experiences it: middleware, approval waits and
    /// scratchpad interception are inside the measured span. A scratchpad
    /// read is left out, as the event surface leaves it out: it is context
    /// management, not a tool call the model made.
    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let name = self.0.name();
            if !StreamingRequestHook::should_publish_tool_event(&name) {
                return self.0.call(args).await;
            }
            let started = std::time::Instant::now();
            let result = self.0.call(args).await;
            crate::metrics::record_tool_execution(&name, error_type(&result), started.elapsed());
            result
        })
    }
}

/// `error.type` for one call: a tool that answered with an error text is
/// told from one that failed, and a failure keeps rig's own kind. Rig
/// JSON-encodes a tool's output, so a string output is read through its
/// quotes.
fn error_type(result: &Result<String, ToolError>) -> Option<&'static str> {
    match result {
        Ok(output) => {
            let decoded = serde_json::from_str::<String>(output).ok();
            let text = decoded.as_deref().unwrap_or(output);
            is_reported_error(text).then_some("tool_error")
        }
        Err(ToolError::ToolCallError(_)) => Some("tool_call_error"),
        Err(ToolError::JsonError(_)) => Some("json_error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_type_tells_answered_errors_from_failures() {
        assert_eq!(error_type(&Ok("{\"items\": []}".to_string())), None);
        assert_eq!(
            error_type(&Ok(serde_json::to_string(
                "Tool returned an error: forbidden"
            )
            .unwrap())),
            Some("tool_error")
        );
        assert_eq!(
            error_type(&Err(ToolError::ToolCallError("boom".into()))),
            Some("tool_call_error")
        );
        let bad_json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert_eq!(
            error_type(&Err(ToolError::JsonError(bad_json))),
            Some("json_error")
        );
    }

    /// The wrapper itself, which needs the instruments to exist.
    #[cfg(feature = "otel")]
    mod metered {
        use super::*;
        use rig::tool::Tool;
        use serde::{Deserialize, Serialize};

        #[derive(Deserialize)]
        struct Args {
            fail: bool,
        }

        #[derive(Serialize)]
        struct Out(String);

        #[derive(Debug, thiserror::Error)]
        #[error("asked to fail")]
        struct Asked;

        /// A tool under a name of the caller's choosing, failing on request.
        struct Named(&'static str);

        impl Tool for Named {
            const NAME: &'static str = "named";
            type Error = Asked;
            type Args = Args;
            type Output = Out;

            fn name(&self) -> String {
                self.0.to_string()
            }

            async fn definition(&self, _prompt: String) -> ToolDefinition {
                ToolDefinition {
                    name: self.0.to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }

            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                if args.fail {
                    Err(Asked)
                } else {
                    Ok(Out("done".into()))
                }
            }
        }

        #[tokio::test]
        async fn metered_tool_records_each_call_with_its_outcome() {
            crate::metrics::init(false).expect("metrics init");
            let tool = MeteredTool::new(Box::new(Named("metered_probe")));

            tool.call(r#"{"fail": false}"#.to_string())
                .await
                .expect("the tool answers");
            tool.call(r#"{"fail": true}"#.to_string())
                .await
                .expect_err("the tool fails on request");
            tool.call("not json".to_string())
                .await
                .expect_err("rig rejects the arguments");

            let text = crate::metrics::render_prometheus().expect("render");
            for needle in [
                r#"gen_ai_execute_tool_duration_seconds_count{gen_ai_tool_name="metered_probe"} 1"#,
                r#"gen_ai_execute_tool_duration_seconds_count{error_type="tool_call_error",gen_ai_tool_name="metered_probe"} 1"#,
                r#"gen_ai_execute_tool_duration_seconds_count{error_type="json_error",gen_ai_tool_name="metered_probe"} 1"#,
            ] {
                assert!(text.contains(needle), "missing {needle} in:\n{text}");
            }
        }

        /// A scratchpad read goes through unmetered, as it goes through the
        /// event surface unpublished.
        #[tokio::test]
        async fn metered_tool_leaves_scratchpad_reads_out() {
            crate::metrics::init(false).expect("metrics init");
            let name = crate::scratchpad::all_tool_definitions()
                .into_iter()
                .next()
                .expect("the scratchpad has tools")
                .name;
            assert!(crate::scratchpad::is_scratchpad_tool(&name));
            let name: &'static str = Box::leak(name.into_boxed_str());
            let tool = MeteredTool::new(Box::new(Named(name)));

            tool.call(r#"{"fail": false}"#.to_string())
                .await
                .expect("the tool answers");

            let text = crate::metrics::render_prometheus().expect("render");
            assert!(
                !text.contains(&format!("gen_ai_tool_name=\"{name}\"")),
                "scratchpad tool {name} must not be metered:\n{text}"
            );
        }
    }
}
