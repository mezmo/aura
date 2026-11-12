//! Serves one chat completion through both chat paths and compares the SSE
//! body and the `agent.stream` span that each produces. The request-bound
//! path is `execute_completion` reading the agent's item stream. The runtime
//! path is `AgentRuntime` writing the session's journal, which `sse_body`
//! then reads. Both paths run the same script on the same mock agent. The
//! request-bound path is the oracle, and it runs live in each test, so no
//! expected output is recorded.
//!
//! Each script has three tests:
//!
//! - `original_path::<script>` runs only the oracle, twice. Its body must
//!   hold the frames the script says it covers, and the two runs must render
//!   the same.
//! - `runtime_span::<script>` compares the runtime path's span with the
//!   oracle's. It reads the session until the run ends and builds no body.
//! - `both_paths::<script>` runs both paths through to the body and compares
//!   them: the bodies byte for byte and the spans as trees.
//!
//! On a mismatch the test writes both sides under
//! `CARGO_TARGET_TMPDIR/sse_parity/`. `sse_parity/MANIFEST.md` lists what
//! each script covers, what no script covers and why, what is rewritten
//! before comparing, and what a pass proves.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use aura::hitl::PendingApprovals;
use aura::runtime::{
    AgentForRun, AgentRuntime, Attach, Caller, RunTarget, RuntimeStores, StartRun, Subscription,
};
use aura::stream_events::{CorrelationContext, EmittingAgent, McpServerStatus};
use aura::test_span_capture::CapturedSpans;
use aura::{
    FinalResponseInfo, PASSTHROUGH_MARKER, ProgressToken, ResponseContent, RunId, SessionId,
    StreamItem, StreamedAssistantContent, StreamingAgent, ToolCallId, ToolName,
};
use aura_events::agent::{AgentEvent, AgentEventPayload, ResultFormat, ToolOutcome};
use aura_events::run::{LifecycleEvent, SessionEventPayload};
use aura_test_utils::mock_agent::{MockAgent, Step, items};
use aura_web_server::handlers::{CompletionConfig, DeliveryMode, RequestSetup, execute_completion};
use aura_web_server::streaming::{
    SseProjector, StreamConfig, Timeouts, ToolResultMode, TurnContext, sse_body,
};
use aura_web_server::types::ActiveRequestTracker;
use futures::{FutureExt, StreamExt};
use opentelemetry::{Array, StringValue, Value};
use tracing::Instrument;

const PROMPT: &str = "What is in /mock?";
const SESSION: &str = "cs_parity";
const COMPLETION_ID: &str = "chatcmpl-test";
const CREATED: u64 = 1_700_000_000;
/// The mock's provider and model, joined with `/` as both paths join them.
const MODEL: &str = "test/fake";
const TRACE: &str = "7061726974790000";
const USER: &str = "parity-user";
const TOOL_RESULT_MAX_LENGTH: usize = 48;
/// A heartbeat interval longer than any script runs.
const QUIET: Duration = Duration::from_secs(86_400);
/// A timeout for reading a body, longer than any script runs.
const BODY_DEADLINE: Duration = Duration::from_secs(3_600);

/// The stream settings a request runs under: custom events, reasoning, the
/// tool result mode and debug provider errors.
#[derive(Debug, Clone, Copy)]
struct Variant {
    custom_events: bool,
    reasoning: bool,
    mode: ToolResultMode,
    debug_provider_errors: bool,
}

impl Variant {
    const fn custom(custom_events: bool) -> Self {
        Self {
            custom_events,
            reasoning: false,
            mode: ToolResultMode::None,
            debug_provider_errors: false,
        }
    }

    const fn mode(self, mode: ToolResultMode) -> Self {
        Self { mode, ..self }
    }

    const fn reasoning(self) -> Self {
        Self {
            reasoning: true,
            ..self
        }
    }

    const fn debug_errors(self) -> Self {
        Self {
            debug_provider_errors: true,
            ..self
        }
    }

    fn label(self) -> String {
        format!(
            "custom-{}_reasoning-{}_mode-{:?}_debug-{}",
            self.custom_events, self.reasoning, self.mode, self.debug_provider_errors
        )
    }
}

const PLAIN: &[Variant] = &[Variant::custom(true), Variant::custom(false)];

/// The tool result modes change the tool frames, so every tool script runs
/// under each mode.
const TOOLS: &[Variant] = &[
    Variant::custom(true),
    Variant::custom(true).mode(ToolResultMode::OpenWebUI),
    Variant::custom(true).mode(ToolResultMode::Aura),
    Variant::custom(false),
    Variant::custom(false).mode(ToolResultMode::OpenWebUI),
    Variant::custom(false).mode(ToolResultMode::Aura),
];

const REASONING: &[Variant] = &[
    Variant::custom(true).reasoning(),
    Variant::custom(true),
    Variant::custom(false).reasoning(),
];

const DEBUG_ERRORS: &[Variant] = &[
    Variant::custom(true),
    Variant::custom(true).debug_errors(),
    Variant::custom(false),
    Variant::custom(false).debug_errors(),
];

/// Whether a single agent or an orchestrator serves the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Single,
    Orchestrated,
}

/// How a script's run ends, as far as its span is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanEnding {
    /// Any ending but a park.
    Same,
    /// A parked run.
    Parked,
}

/// A check on the oracle's body: text it must hold, text it must not hold,
/// or two texts in order.
enum Mark {
    Has(String),
    Lacks(String),
    /// Both texts occur, and the first comes before the second.
    Before(String, String),
}

fn has(text: impl Into<String>) -> Mark {
    Mark::Has(text.into())
}

fn lacks(text: impl Into<String>) -> Mark {
    Mark::Lacks(text.into())
}

fn event_frame(name: &str) -> String {
    format!("event: {name}\n")
}

/// One scripted run: what the agent does and how the request is set up.
#[derive(Clone, Copy)]
struct Script {
    name: &'static str,
    shape: Shape,
    steps: fn() -> Vec<Step>,
    mcp_status: fn() -> Vec<McpServerStatus>,
    client_tools: bool,
    max_tokens: Option<u32>,
    variants: &'static [Variant],
    covers: fn(Variant) -> Vec<Mark>,
    ending: SpanEnding,
}

impl Script {
    fn new(
        name: &'static str,
        shape: Shape,
        steps: fn() -> Vec<Step>,
        variants: &'static [Variant],
        covers: fn(Variant) -> Vec<Mark>,
    ) -> Self {
        Self {
            name,
            shape,
            steps,
            mcp_status: Vec::new,
            client_tools: false,
            max_tokens: None,
            variants,
            covers,
            ending: SpanEnding::Same,
        }
    }

    fn agent(self, run: RunId) -> MockAgent {
        MockAgent::scripted((self.steps)())
            .for_run(run)
            .with_run_context()
            .with_mcp_status((self.mcp_status)())
    }

    fn attribution(self) -> EmittingAgent {
        match self.shape {
            Shape::Single => EmittingAgent::single_agent(),
            Shape::Orchestrated => EmittingAgent::coordinator(),
        }
    }

    fn config(self) -> aura_config::Config {
        let orchestration = match self.shape {
            Shape::Single => "",
            Shape::Orchestrated => {
                "[orchestration]\nenabled = true\n\n\
                 [orchestration.worker.researcher]\n\
                 description = \"Looks things up\"\npreamble = \"p\"\n"
            }
        };
        aura_config::load_config_from_str(&format!(
            "[agent]\nname = \"parity\"\nsystem_prompt = \"p\"\n\n\
             [agent.llm]\nprovider = \"openai\"\napi_key = \"test-key\"\n\
             model = \"gpt-4o\"\ntemperature = 0.2\n\n{orchestration}"
        ))
        .expect("the parity config parses")
    }

    fn stream_config(self, variant: Variant) -> StreamConfig {
        StreamConfig::new(
            variant.custom_events,
            variant.reasoning,
            variant.mode,
            TOOL_RESULT_MAX_LENGTH,
        )
        .with_client_tools(self.client_tools)
        .with_debug_provider_errors(variant.debug_provider_errors)
    }

    fn turn(self) -> TurnContext {
        TurnContext {
            completion_id: COMPLETION_ID.to_owned(),
            model_str: MODEL.to_owned(),
            created_timestamp: CREATED,
            max_tokens: self.max_tokens,
            emitting_agent: self.attribution(),
            correlation: CorrelationContext::new(SESSION, Some(TRACE.to_owned())),
        }
    }
}

fn session() -> SessionId {
    SessionId::new(SESSION).expect("the parity session id is not empty")
}

fn metadata() -> std::collections::HashMap<String, String> {
    [("tenant".to_owned(), "parity".to_owned())].into()
}

/// Helpers that build the steps the scripts are made of.
mod steps {
    use super::*;

    pub const TOOL_ID: &str = "call_parity_1";
    pub const SECOND_TOOL_ID: &str = "call_parity_2";
    pub const THIRD_TOOL_ID: &str = "call_parity_3";
    pub const ARGS: &str = r#"{"limit":10,"path":"/mock"}"#;
    pub const FETCH_ARGS: &str = r#"{"url":"https://mock"}"#;
    /// Arguments cut short, so they do not parse.
    pub const CUT_ARGS: &str = r#"{"path":"/mo"#;
    /// Longer than `TOOL_RESULT_MAX_LENGTH`.
    pub const LONG_RESULT: &str =
        "a.txt b.txt c.txt d.txt e.txt f.txt g.txt h.txt i.txt j.txt k.txt";
    pub const WORKER: &str = "researcher";
    pub const ORCHESTRATOR_ID: &str = "orchestrator_parity";
    pub const DECISION: &str = "0192f1d2-7c4e-7a10-8000-000000000001";

    pub fn text(content: &str) -> Step {
        Step::item(Ok(text_item(content)))
    }

    pub fn text_item(content: &str) -> StreamItem {
        StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(content.to_owned()))
    }

    pub fn reasoning(delta: &str) -> Step {
        Step::item(Ok(reasoning_item(delta)))
    }

    pub fn reasoning_item(delta: &str) -> StreamItem {
        StreamItem::StreamAssistantItem(StreamedAssistantContent::ReasoningDelta {
            id: None,
            delta: delta.to_owned(),
        })
    }

    pub fn final_response(content: &str, input: u64, output: u64) -> Step {
        final_with_cache(content, input, output, None)
    }

    pub fn final_with_cache(
        content: &str,
        input: u64,
        output: u64,
        cache: Option<(u64, u64)>,
    ) -> Step {
        Step::item(Ok(StreamItem::Final(FinalResponseInfo {
            content: content.to_owned(),
            usage: usage(input, output),
            cache_usage: cache.map(|(read, creation)| rig::completion::CacheUsage {
                cache_read_input_tokens: read,
                cache_creation_input_tokens: creation,
            }),
        })))
    }

    pub fn final_item(content: &str, input: u64, output: u64) -> StreamItem {
        StreamItem::Final(FinalResponseInfo {
            content: content.to_owned(),
            usage: usage(input, output),
            cache_usage: None,
        })
    }

    pub fn usage(input: u64, output: u64) -> rig::completion::Usage {
        rig::completion::Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
        }
    }

    pub fn error(text: &'static str) -> Step {
        Step::item(Err(text.into()))
    }

    /// A step in which the single agent sends `payload` on the run's event
    /// channel.
    pub fn event(payload: AgentEventPayload) -> Step {
        event_as(EmittingAgent::single_agent(), payload)
    }

    pub fn event_as(agent: EmittingAgent, payload: AgentEventPayload) -> Step {
        let event = AgentEvent::new(agent, payload);
        Step::effect(move |_| {
            let event = event.clone();
            async move {
                aura::run_context::emit(event).await;
            }
        })
    }

    /// A step that does what one call to the orchestrator's emitter does. It
    /// yields `item` on the item stream and sends the matching event on the
    /// run's channel, attributed the way the emitter attributes it.
    pub fn emitted(item: StreamItem) -> Step {
        let event = match &item {
            StreamItem::AgentEvent(event) => AgentEvent::clone(event),
            StreamItem::StreamAssistantItem(StreamedAssistantContent::Text(content)) => {
                AgentEvent::new(
                    EmittingAgent::coordinator(),
                    AgentEventPayload::TextDelta {
                        content: content.clone(),
                    },
                )
            }
            StreamItem::StreamAssistantItem(StreamedAssistantContent::ReasoningDelta {
                delta,
                ..
            }) => AgentEvent::new(
                EmittingAgent::coordinator(),
                AgentEventPayload::Reasoning {
                    content: delta.clone(),
                    task_id: None,
                },
            ),
            StreamItem::Final(response) => AgentEvent::new(
                EmittingAgent::coordinator(),
                AgentEventPayload::Completed {
                    content: response.content.clone(),
                    usage: token_usage(response.usage.input_tokens, response.usage.output_tokens),
                },
            ),
            StreamItem::ContextUsage {
                agent_id,
                context_tokens,
                response_tokens,
                context_window,
            } => AgentEvent::new(
                EmittingAgent::orchestrated(agent_id),
                AgentEventPayload::ContextUsage {
                    context_tokens: aura_events::TokenCount::new(*context_tokens),
                    response_tokens: aura_events::TokenCount::new(*response_tokens),
                    context_window: context_window.map(aura_events::TokenCount::new),
                },
            ),
            StreamItem::ScratchpadUsage {
                agent_id,
                tokens_intercepted,
                tokens_extracted,
            } => AgentEvent::new(
                EmittingAgent::orchestrated(agent_id),
                AgentEventPayload::ScratchpadUsage {
                    tokens_intercepted: aura_events::TokenCount::new(*tokens_intercepted as u64),
                    tokens_extracted: aura_events::TokenCount::new(*tokens_extracted as u64),
                },
            ),
            StreamItem::McpStatus(servers) => AgentEvent::new(
                EmittingAgent::coordinator(),
                AgentEventPayload::McpStatus {
                    servers: servers.clone(),
                },
            ),
            other => panic!("the orchestrator's emitter carries no event for {other:?}"),
        };
        Step::emitted(Ok(item), move |_| {
            let event = event.clone();
            async move {
                aura::run_context::emit(event).await;
            }
        })
    }

    pub fn by_coordinator(payload: AgentEventPayload) -> StreamItem {
        StreamItem::AgentEvent(Box::new(AgentEvent::new(
            EmittingAgent::coordinator(),
            payload,
        )))
    }

    pub fn by_worker(payload: AgentEventPayload) -> StreamItem {
        StreamItem::AgentEvent(Box::new(AgentEvent::new(
            EmittingAgent::orchestrated(WORKER),
            payload,
        )))
    }

    pub fn tool_requested(id: &str, name: &str, arguments: &str) -> Step {
        event(AgentEventPayload::ToolRequested {
            tool_call_id: ToolCallId::new(id),
            tool_name: ToolName::new(name),
            arguments: serde_json::from_str(arguments).expect("the arguments are JSON"),
            unparsed_arguments: None,
        })
    }

    pub fn tool_start(id: &str, name: &str, progress: Option<i64>) -> Step {
        event(AgentEventPayload::ToolStart {
            tool_call_id: ToolCallId::new(id),
            tool_name: ToolName::new(name),
            progress_token: progress
                .map(|token| ProgressToken(aura::NumberOrString::Number(token))),
            arguments: None,
            task_id: None,
        })
    }

    pub fn token_usage(input: u64, output: u64) -> aura_events::TokenUsage {
        aura_events::TokenUsage {
            prompt_tokens: aura_events::TokenCount::new(input),
            completion_tokens: aura_events::TokenCount::new(output),
            total_tokens: aura_events::TokenCount::new(input + output),
        }
    }

    pub fn plan_created() -> AgentEventPayload {
        AgentEventPayload::PlanCreated {
            goal: "List and summarise /mock".to_owned(),
            tasks: vec!["List the files".to_owned(), "Summarise them".to_owned()],
            routing_mode: aura::RoutingMode::Orchestrated,
            routing_rationale: "Two steps".to_owned(),
            planning_response: "I will list, then summarise.".to_owned(),
        }
    }

    pub fn task_started() -> AgentEventPayload {
        AgentEventPayload::TaskStarted {
            task_id: 0,
            description: "List the files".to_owned(),
            orchestrator_id: ORCHESTRATOR_ID.to_owned(),
        }
    }

    pub fn single_scope() -> aura_events::AgentScopeWire {
        aura_events::AgentScopeWire::Single {
            session_id: Some(SESSION.to_owned()),
        }
    }
}

/// The scripts. Each one lists the frames its oracle body must hold, so the
/// test checks the manifest's claim that a script covers a surface.
mod scripts {
    use super::steps::*;
    use super::*;
    use aura_events::event_names as aura_names;
    use aura_events::orchestration::event_names as orchestrator_names;

    /// Returns `marks` when the variant has custom events on. With custom
    /// events off, the only check is that the body has no
    /// `aura.session_info` frame.
    fn custom_events_only(variant: Variant, marks: Vec<Mark>) -> Vec<Mark> {
        if variant.custom_events {
            marks
        } else {
            vec![lacks(event_frame(aura_names::SESSION_INFO))]
        }
    }

    pub fn text_and_final() -> Script {
        Script::new(
            "text_and_final",
            Shape::Single,
            || {
                vec![
                    text("Hello"),
                    text(", world."),
                    final_response("Hello, world.", 12, 5),
                ]
            },
            PLAIN,
            |variant| {
                let mut marks = vec![
                    has(r#""delta":{"role":"assistant","content":"Hello"}"#),
                    has(r#""delta":{"content":", world."}"#),
                    has(r#""finish_reason":"stop""#),
                    has(r#""usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}"#),
                ];
                marks.extend(custom_events_only(
                    variant,
                    vec![
                        has(event_frame(aura_names::SESSION_INFO)),
                        has(event_frame(aura_names::USAGE)),
                    ],
                ));
                if !variant.custom_events {
                    marks.push(lacks("event: aura."));
                }
                marks
            },
        )
    }

    pub fn tool_turn() -> Script {
        Script::new(
            "tool_turn",
            Shape::Single,
            || {
                vec![
                    text("Let me look."),
                    Step::item(items::tool_call(TOOL_ID, "list_files", ARGS)),
                    tool_requested(TOOL_ID, "list_files", ARGS),
                    tool_start(TOOL_ID, "list_files", Some(7)),
                    event(AgentEventPayload::ToolProgress {
                        progress_token: ProgressToken(aura::NumberOrString::Number(7)),
                        progress: aura_events::Progress::ratio(50.0, 100.0),
                        message: Some("halfway".to_owned()),
                    }),
                    Step::item(items::tool_result(TOOL_ID, LONG_RESULT)),
                    Step::item(Ok(StreamItem::TurnUsage(usage(30, 8), None))),
                    text("Here are the files."),
                    event(AgentEventPayload::ToolUsage {
                        tool_call_ids: vec![ToolCallId::new(TOOL_ID)],
                        usage: token_usage(30, 8),
                    }),
                    final_response("Let me look.\n\nHere are the files.", 40, 12),
                ]
            },
            TOOLS,
            |variant| {
                let mut marks = vec![
                    has(
                        r#""tool_calls":[{"index":0,"id":"call_parity_1","type":"function","function":{"name":"list_files","#,
                    ),
                    has(r#""content":"\n\nHere are the files.""#),
                ];
                match variant.mode {
                    ToolResultMode::OpenWebUI => {
                        marks.push(has(r#""name":"list_files","arguments":"""#));
                        marks.push(has(r#""function":{"name":"","arguments":"\"a.txt"#));
                    }
                    ToolResultMode::None | ToolResultMode::Aura => {
                        marks.push(has(
                            r#""name":"list_files","arguments":"{\"limit\":10,\"path\":\"/mock\"}""#,
                        ));
                        marks.push(lacks(r#""function":{"name":"""#));
                    }
                }
                marks.extend(custom_events_only(
                    variant,
                    vec![
                        has(event_frame(aura_names::TOOL_REQUESTED)),
                        has(event_frame(aura_names::TOOL_START)),
                        has(event_frame(aura_names::PROGRESS)),
                        has(event_frame(aura_names::TOOL_COMPLETE)),
                        has(event_frame(aura_names::TOOL_USAGE)),
                        has("... [truncated]"),
                        Mark::Before(
                            r#""tool_calls":[{"index":0"#.to_owned(),
                            event_frame(aura_names::TOOL_REQUESTED),
                        ),
                    ],
                ));
                if !variant.custom_events {
                    marks.push(lacks("event: aura."));
                }
                marks
            },
        )
    }

    /// Three tool calls, one for each kind of result: an error, output that
    /// reports an error, and JSON output.
    pub fn tool_outcomes() -> Script {
        Script::new(
            "tool_outcomes",
            Shape::Single,
            || {
                vec![
                    Step::item(items::tool_call(TOOL_ID, "read_file", ARGS)),
                    tool_requested(TOOL_ID, "read_file", ARGS),
                    Step::item(items::tool_error(
                        TOOL_ID,
                        "Toolset error: ToolCallError: ToolCallError: connection refused",
                    )),
                    Step::item(items::tool_call(SECOND_TOOL_ID, "fetch", FETCH_ARGS)),
                    tool_requested(SECOND_TOOL_ID, "fetch", FETCH_ARGS),
                    Step::item(items::tool_result(
                        SECOND_TOOL_ID,
                        "Tool returned an error: 401 Unauthorized",
                    )),
                    Step::item(items::tool_call(THIRD_TOOL_ID, "stat", ARGS)),
                    tool_requested(THIRD_TOOL_ID, "stat", ARGS),
                    Step::item(Ok(StreamItem::StreamUserItem(
                        aura::StreamedUserContent::ToolResult(aura::ToolResult {
                            id: THIRD_TOOL_ID.to_owned(),
                            call_id: None,
                            result: r#"{"files":11}"#.to_owned(),
                            kind: aura::ToolResultKind::Json,
                        }),
                    ))),
                    text("Two tools failed."),
                    final_response("Two tools failed.", 22, 4),
                ]
            },
            TOOLS,
            |variant| {
                let mut marks = vec![
                    has(r#""tool_calls":[{"index":1,"id":"call_parity_2""#),
                    has(r#""tool_calls":[{"index":2,"id":"call_parity_3""#),
                ];
                if variant.mode == ToolResultMode::OpenWebUI {
                    marks.push(has(r#""arguments":"\"connection refused\"""#));
                    marks.push(has(r#""arguments":"{\"files\":11}""#));
                }
                marks.extend(custom_events_only(
                    variant,
                    vec![
                        has(r#""success":false"#),
                        has(r#""error":"connection refused""#),
                        has(r#""error":"Tool returned an error: 401 Unauthorized""#),
                        has(r#""result":"{\"files\":11}""#),
                    ],
                ));
                marks
            },
        )
    }

    /// A call whose arguments do not parse as JSON, which happens when a
    /// provider cuts them short. The tool call item has the raw text. The
    /// streaming hook's `ToolRequested` has an empty object as its arguments
    /// and the raw text in `unparsed_arguments`.
    pub fn unparsed_tool_arguments() -> Script {
        Script::new(
            "unparsed_tool_arguments",
            Shape::Single,
            || {
                vec![
                    Step::item(items::tool_call(TOOL_ID, "list_files", CUT_ARGS)),
                    event(AgentEventPayload::ToolRequested {
                        tool_call_id: ToolCallId::new(TOOL_ID),
                        tool_name: ToolName::new("list_files"),
                        arguments: serde_json::json!({}),
                        unparsed_arguments: Some(CUT_ARGS.to_owned()),
                    }),
                    Step::item(items::tool_result(TOOL_ID, "a.txt")),
                    text("One file."),
                    final_response("One file.", 18, 3),
                ]
            },
            TOOLS,
            |variant| {
                let mut marks = Vec::new();
                if variant.mode != ToolResultMode::OpenWebUI {
                    marks.push(has(r#""name":"list_files","arguments":"{\"path\":\"/mo""#));
                }
                marks.extend(custom_events_only(
                    variant,
                    vec![has(r#""tool_name":"list_files","arguments":{}"#)],
                ));
                marks
            },
        )
    }

    /// An Ollama model's tool call written as text, in the order the
    /// fallback executor gives it: the text, then at the end of the turn the
    /// call's item, its `ToolRequested` and its result, then `Final`. The
    /// executor calls MCP without a tool call id, so there is no `ToolStart`.
    pub fn fallback_tool_call() -> Script {
        const WRITTEN: &str =
            r#"{"name": "list_files", "parameters": {"limit": 10, "path": "/mock"}}"#;
        Script::new(
            "fallback_tool_call",
            Shape::Single,
            || {
                vec![
                    text(WRITTEN),
                    Step::item(items::tool_call(TOOL_ID, "list_files", ARGS)),
                    tool_requested(TOOL_ID, "list_files", ARGS),
                    Step::item(items::tool_result(TOOL_ID, "a.txt")),
                    final_response(WRITTEN, 20, 9),
                ]
            },
            TOOLS,
            |variant| {
                let mut marks = vec![has(
                    r#""tool_calls":[{"index":0,"id":"call_parity_1","type":"function","function":{"name":"list_files","#,
                )];
                marks.extend(custom_events_only(
                    variant,
                    vec![
                        has(event_frame(aura_names::TOOL_REQUESTED)),
                        has(event_frame(aura_names::TOOL_COMPLETE)),
                        lacks(event_frame(aura_names::TOOL_START)),
                        Mark::Before(
                            r#""tool_calls":[{"index":0"#.to_owned(),
                            event_frame(aura_names::TOOL_REQUESTED),
                        ),
                    ],
                ));
                marks
            },
        )
    }

    pub fn reasoning_then_text() -> Script {
        Script::new(
            "reasoning_then_text",
            Shape::Single,
            || {
                vec![
                    reasoning("Thinking about it."),
                    reasoning(" Done."),
                    text("Answer."),
                    final_response("Answer.", 9, 3),
                ]
            },
            REASONING,
            |variant| {
                let reasoning_frame = event_frame(aura_names::REASONING);
                let mut marks = vec![has(r#""delta":{"role":"assistant","content":"Answer."}"#)];
                if variant.custom_events && variant.reasoning {
                    marks.push(has(reasoning_frame));
                    marks.push(has("Thinking about it."));
                } else {
                    marks.push(lacks(reasoning_frame));
                }
                marks
            },
        )
    }

    pub fn final_with_cache_split() -> Script {
        Script::new(
            "final_with_cache_split",
            Shape::Single,
            || {
                vec![
                    text("Cached."),
                    final_with_cache("Cached.", 100, 7, Some((80, 4))),
                ]
            },
            PLAIN,
            |variant| {
                custom_events_only(
                    variant,
                    vec![
                        has(event_frame(aura_names::USAGE)),
                        has(r#""cache_read_input_tokens":80"#),
                        has(r#""cache_creation_input_tokens":4"#),
                    ],
                )
            },
        )
    }

    pub fn provider_error() -> Script {
        Script::new(
            "provider_error",
            Shape::Single,
            || vec![text("Partial"), error("upstream returned 502 Bad Gateway")],
            DEBUG_ERRORS,
            |variant| {
                let mut marks = vec![has(r#""finish_reason":"stop""#), has("data: [DONE]")];
                if variant.debug_provider_errors {
                    marks.push(has(
                        "The upstream model provider (test/fake) returned an error: upstream returned 502 Bad Gateway",
                    ));
                } else {
                    marks.push(has(
                        "The upstream model provider (test/fake) returned an error and the request could not be completed.",
                    ));
                    marks.push(lacks("502"));
                }
                marks
            },
        )
    }

    pub fn context_overflow() -> Script {
        Script::new(
            "context_overflow",
            Shape::Single,
            || {
                vec![error(
                    "context_length_exceeded: this model's maximum context length is 8192 tokens",
                )]
            },
            PLAIN,
            |_| {
                vec![
                    has(
                        r#""delta":{"role":"assistant","content":"My tools returned more data than I can work with at once."#,
                    ),
                    has(r#""finish_reason":"stop""#),
                ]
            },
        )
    }

    pub fn passthrough_tool() -> Script {
        Script {
            client_tools: true,
            ..Script::new(
                "passthrough_tool",
                Shape::Single,
                || {
                    vec![
                        text("Checking the weather."),
                        Step::item(items::tool_call(
                            TOOL_ID,
                            "get_weather",
                            r#"{"city":"Paris"}"#,
                        )),
                        tool_requested(TOOL_ID, "get_weather", r#"{"city":"Paris"}"#),
                        Step::item(items::tool_result(
                            TOOL_ID,
                            &format!("\"{PASSTHROUGH_MARKER}\""),
                        )),
                        error("the run stopped so the client can run its tool"),
                    ]
                },
                TOOLS,
                |_| {
                    vec![
                        has(
                            r#""tool_calls":[{"index":0,"id":"call_parity_1","type":"function","function":{"name":"get_weather","#,
                        ),
                        has(r#""finish_reason":"tool_calls""#),
                        lacks(PASSTHROUGH_MARKER),
                        lacks(event_frame(aura_names::TOOL_COMPLETE)),
                    ]
                },
            )
        }
    }

    pub fn max_tokens_length() -> Script {
        Script {
            max_tokens: Some(5),
            ..Script::new(
                "max_tokens_length",
                Shape::Single,
                || vec![text("Cut short"), final_response("Cut short", 20, 5)],
                PLAIN,
                |_| vec![has(r#""finish_reason":"length""#)],
            )
        }
    }

    pub fn approvals() -> Script {
        Script::new(
            "approvals",
            Shape::Single,
            || {
                vec![
                    Step::item(items::tool_call(TOOL_ID, "delete_file", ARGS)),
                    tool_requested(TOOL_ID, "delete_file", ARGS),
                    event(AgentEventPayload::ApprovalRequested(
                        aura_events::ApprovalRequested {
                            decision_id: DECISION.to_owned(),
                            tool_name: "delete_file".to_owned(),
                            tool_namespace: Some("files".to_owned()),
                            origin: aura_events::ApprovalOriginWire::ConfigGate {
                                matched_pattern: "files.delete_*".to_owned(),
                                agent_name: "parity".to_owned(),
                            },
                            scope: single_scope(),
                        },
                    )),
                    event(AgentEventPayload::ApprovalPending(
                        aura_events::ApprovalPending {
                            decision_id: DECISION.to_owned(),
                            tool_name: "delete_file".to_owned(),
                            tool_namespace: Some("files".to_owned()),
                            arguments: serde_json::from_str(ARGS).expect("the arguments are JSON"),
                            origin: aura_events::ApprovalOriginWire::ConfigGate {
                                matched_pattern: "files.delete_*".to_owned(),
                                agent_name: "parity".to_owned(),
                            },
                            scope: single_scope(),
                            expires_at: "2026-11-03T12:00:00Z".to_owned(),
                        },
                    )),
                    event(AgentEventPayload::ApprovalCompleted(
                        aura_events::ApprovalCompleted {
                            decision_id: DECISION.to_owned(),
                            outcome: aura_events::ApprovalOutcomeWire::Approved,
                            duration_ms: 1_200,
                            scope: single_scope(),
                        },
                    )),
                    tool_start(TOOL_ID, "delete_file", None),
                    Step::item(items::tool_result(TOOL_ID, "deleted")),
                    text("Deleted."),
                    final_response("Deleted.", 15, 2),
                ]
            },
            PLAIN,
            |_| {
                vec![
                    has(event_frame(aura_names::APPROVAL_REQUESTED)),
                    has(event_frame(aura_names::APPROVAL_PENDING)),
                    has(event_frame(aura_names::APPROVAL_COMPLETED)),
                ]
            },
        )
    }

    pub fn mcp_status_at_start() -> Script {
        Script {
            mcp_status: || {
                vec![
                    McpServerStatus {
                        server_name: "files".to_owned(),
                        transport: "stdio".to_owned(),
                        status: "connected".to_owned(),
                        tools_count: 3,
                        reason: None,
                    },
                    McpServerStatus {
                        server_name: "search".to_owned(),
                        transport: "http_streamable".to_owned(),
                        status: "failed".to_owned(),
                        tools_count: 0,
                        reason: Some("server returned HTTP 503 Service Unavailable".to_owned()),
                    },
                ]
            },
            ..Script::new(
                "mcp_status_at_start",
                Shape::Single,
                || vec![text("Ready."), final_response("Ready.", 5, 1)],
                PLAIN,
                |variant| {
                    custom_events_only(
                        variant,
                        vec![
                            has(event_frame(aura_names::MCP_STATUS)),
                            Mark::Before(
                                event_frame(aura_names::SESSION_INFO),
                                event_frame(aura_names::MCP_STATUS),
                            ),
                            Mark::Before(event_frame(aura_names::MCP_STATUS), "Ready.".to_owned()),
                        ],
                    )
                },
            )
        }
    }

    pub fn scratchpad_tail() -> Script {
        Script::new(
            "scratchpad_tail",
            Shape::Single,
            || {
                vec![
                    text("Summarised."),
                    final_response("Summarised.", 50, 3),
                    Step::item(Ok(StreamItem::ScratchpadUsage {
                        agent_id: "main".to_owned(),
                        tokens_intercepted: 1_200,
                        tokens_extracted: 300,
                    })),
                ]
            },
            PLAIN,
            |_| {
                vec![
                    has(event_frame(aura_names::SCRATCHPAD_USAGE)),
                    has(r#""tokens_intercepted":1200"#),
                ]
            },
        )
    }

    /// A scratchpad exploration tool. The streaming hook does not announce
    /// its calls, so the call and its result reach the stream as items, but
    /// no `ToolRequested` reaches the run's channel.
    pub fn scratchpad_tool_hidden() -> Script {
        Script::new(
            "scratchpad_tool_hidden",
            Shape::Single,
            || {
                vec![
                    text("Reading."),
                    Step::item(items::tool_call(TOOL_ID, "head", r#"{"lines":5}"#)),
                    Step::item(items::tool_result(TOOL_ID, "line 1\nline 2")),
                    text(" Read."),
                    final_response("Reading. Read.", 30, 4),
                ]
            },
            TOOLS,
            |_| {
                vec![
                    has(r#""content":" Read.""#),
                    lacks(r#""tool_calls""#),
                    lacks(event_frame(aura_names::TOOL_COMPLETE)),
                ]
            },
        )
    }

    pub fn orchestration_events() -> Script {
        Script {
            mcp_status: Vec::new,
            ..Script::new(
                "orchestration_events",
                Shape::Orchestrated,
                || {
                    vec![
                        emitted(StreamItem::McpStatus(vec![McpServerStatus {
                            server_name: "files".to_owned(),
                            transport: "stdio".to_owned(),
                            status: "connected".to_owned(),
                            tools_count: 3,
                            reason: None,
                        }])),
                        emitted(reasoning_item("Planning the work.")),
                        emitted(by_coordinator(plan_created())),
                        emitted(by_worker(task_started())),
                        emitted(by_worker(AgentEventPayload::ToolStart {
                            tool_call_id: ToolCallId::new(TOOL_ID),
                            tool_name: ToolName::new("list_files"),
                            progress_token: None,
                            arguments: Some(serde_json::from_str(ARGS).expect("JSON")),
                            task_id: Some(0),
                        })),
                        emitted(by_worker(AgentEventPayload::ToolComplete {
                            tool_call_id: ToolCallId::new(TOOL_ID),
                            tool_name: ToolName::new("list_files"),
                            duration_ms: 42,
                            outcome: ToolOutcome::Success {
                                result: LONG_RESULT.to_owned(),
                                format: ResultFormat::Text,
                            },
                            task_id: Some(0),
                        })),
                        emitted(by_worker(AgentEventPayload::Reasoning {
                            content: "Eleven files.".to_owned(),
                            task_id: Some(0),
                        })),
                        emitted(StreamItem::ContextUsage {
                            agent_id: WORKER.to_owned(),
                            context_tokens: 900,
                            response_tokens: 40,
                            context_window: Some(128_000),
                        }),
                        emitted(StreamItem::ScratchpadUsage {
                            agent_id: WORKER.to_owned(),
                            tokens_intercepted: 1_200,
                            tokens_extracted: 300,
                        }),
                        emitted(by_worker(AgentEventPayload::TaskCompleted {
                            task_id: 0,
                            duration_ms: 250,
                            orchestrator_id: ORCHESTRATOR_ID.to_owned(),
                            outcome: ToolOutcome::Success {
                                result: "Listed eleven files".to_owned(),
                                format: ResultFormat::Text,
                            },
                        })),
                        emitted(by_coordinator(AgentEventPayload::IterationComplete {
                            iteration: 1,
                            will_replan: true,
                            reasoning: "One task left".to_owned(),
                            gaps: vec!["No summary yet".to_owned()],
                            timings: aura_events::orchestration::IterationTimings::default(),
                        })),
                        emitted(by_coordinator(AgentEventPayload::ReplanStarted {
                            iteration: 2,
                            trigger: "gaps".to_owned(),
                        })),
                        emitted(by_coordinator(AgentEventPayload::Synthesizing {
                            iteration: 2,
                        })),
                        emitted(text_item("Here is the summary.")),
                        emitted(StreamItem::ContextUsage {
                            agent_id: "main".to_owned(),
                            context_tokens: 2_000,
                            response_tokens: 120,
                            context_window: None,
                        }),
                        emitted(final_item("Here is the summary.", 0, 0)),
                    ]
                },
                REASONING,
                |variant| {
                    let mut marks = vec![
                        has(event_frame(aura_names::CONTEXT_USAGE)),
                        has(event_frame(aura_names::SCRATCHPAD_USAGE)),
                        has(r#""content":"Here is the summary.""#),
                    ];
                    if variant.custom_events {
                        marks.extend([
                            has(event_frame(aura_names::MCP_STATUS)),
                            has(event_frame(orchestrator_names::PLAN_CREATED)),
                            has(event_frame(orchestrator_names::TASK_STARTED)),
                            has(event_frame(orchestrator_names::TOOL_CALL_STARTED)),
                            has(event_frame(orchestrator_names::TOOL_CALL_COMPLETED)),
                            has(event_frame(orchestrator_names::TASK_COMPLETED)),
                            has(event_frame(orchestrator_names::ITERATION_COMPLETE)),
                            has(event_frame(orchestrator_names::REPLAN_STARTED)),
                            has(event_frame(orchestrator_names::SYNTHESIZING)),
                        ]);
                    } else {
                        marks.push(lacks("event: aura.orchestrator."));
                    }
                    if variant.custom_events && variant.reasoning {
                        marks.push(has(event_frame(orchestrator_names::WORKER_REASONING)));
                        marks.push(Mark::Before(
                            "Planning the work.".to_owned(),
                            event_frame(orchestrator_names::PLAN_CREATED),
                        ));
                    }
                    marks
                },
            )
        }
    }

    pub fn orchestration_direct_answer() -> Script {
        Script::new(
            "orchestration_direct_answer",
            Shape::Orchestrated,
            || {
                vec![
                    emitted(by_coordinator(AgentEventPayload::DirectAnswer {
                        response: "Four.".to_owned(),
                        routing_rationale: "Arithmetic".to_owned(),
                    })),
                    emitted(text_item("Four.")),
                    emitted(final_item("Four.", 0, 0)),
                ]
            },
            PLAIN,
            |variant| {
                custom_events_only(
                    variant,
                    vec![has(event_frame(orchestrator_names::DIRECT_ANSWER))],
                )
            },
        )
    }

    pub fn orchestration_clarification() -> Script {
        Script::new(
            "orchestration_clarification",
            Shape::Orchestrated,
            || {
                vec![
                    emitted(by_coordinator(AgentEventPayload::ClarificationNeeded {
                        question: "Which directory?".to_owned(),
                        options: Some(vec!["/mock".to_owned(), "/tmp".to_owned()]),
                        routing_rationale: "Ambiguous".to_owned(),
                    })),
                    emitted(text_item("Which directory?")),
                    emitted(final_item("Which directory?", 0, 0)),
                ]
            },
            PLAIN,
            |variant| {
                custom_events_only(
                    variant,
                    vec![has(event_frame(orchestrator_names::CLARIFICATION_NEEDED))],
                )
            },
        )
    }

    pub fn orchestration_parks() -> Script {
        Script {
            ending: SpanEnding::Parked,
            ..Script::new(
                "orchestration_parks",
                Shape::Orchestrated,
                || {
                    vec![
                        emitted(by_coordinator(plan_created())),
                        emitted(by_worker(task_started())),
                        emitted(by_worker(AgentEventPayload::TaskBlocked {
                            task_id: 0,
                            tool_call_id: ToolCallId::new(TOOL_ID),
                            decision_id: DECISION.to_owned(),
                            tool_name: ToolName::new("delete_file"),
                            orchestrator_id: ORCHESTRATOR_ID.to_owned(),
                        })),
                        emitted(by_coordinator(AgentEventPayload::RunParked {
                            run_id: "run:parity".to_owned(),
                            decision_ids: vec![DECISION.to_owned()],
                            expires_at: "2026-11-03T12:00:00Z".to_owned(),
                            iteration: 1,
                        })),
                    ]
                },
                PLAIN,
                |variant| {
                    let mut marks = vec![has(r#""finish_reason":"stop""#), has("data: [DONE]")];
                    marks.extend(custom_events_only(
                        variant,
                        vec![
                            has(event_frame(orchestrator_names::TASK_BLOCKED)),
                            has(event_frame(orchestrator_names::RUN_PARKED)),
                        ],
                    ));
                    marks
                },
            )
        }
    }

    /// The orchestrator sends an event while content it sent earlier is
    /// still buffered on the item stream. The content went on the run's
    /// channel when it was sent. The event goes on the channel before its own
    /// item is read from the stream. So the channel has the content first and
    /// the event second, the same order as the item stream.
    pub fn orchestration_event_ahead_of_content() -> Script {
        Script::new(
            "orchestration_event_ahead_of_content",
            Shape::Orchestrated,
            || {
                vec![
                    emitted(reasoning_item("Planning the work.")),
                    event_as(EmittingAgent::coordinator(), plan_created()),
                    Step::item(Ok(by_coordinator(plan_created()))),
                    emitted(text_item("Done.")),
                    emitted(final_item("Done.", 0, 0)),
                ]
            },
            REASONING,
            |variant| {
                if variant.custom_events && variant.reasoning {
                    vec![Mark::Before(
                        "Planning the work.".to_owned(),
                        event_frame(orchestrator_names::PLAN_CREATED),
                    )]
                } else {
                    vec![has(r#""content":"Done.""#)]
                }
            },
        )
    }
}

/// What one path produced for one run, normalised for comparison.
struct Rendered {
    body: String,
    spans: String,
}

/// Runs the request-bound path: `execute_completion` over the mock, with a
/// hand-built setup and config, inside an `agent.stream` span like the one
/// the streaming handler opens.
async fn request_bound(script: Script, variant: Variant) -> Rendered {
    let captured = CapturedSpans::default();
    let _subscriber = captured.install();

    let run = RunId::mint();
    let agent: Arc<dyn StreamingAgent> = Arc::new(script.agent(run));
    let config = script.config();
    let (provider, model) = agent.get_provider_info();
    let completion = CompletionConfig {
        request_id: run.to_string(),
        timeout_duration: None,
        first_chunk_timeout: None,
        inactivity_timeout: None,
        stream_config: script.stream_config(variant),
        turn_context: script.turn(),
        stream_shutdown_token: tokio_util::sync::CancellationToken::new(),
        active_requests: Arc::new(ActiveRequestTracker::new()),
        provider: provider.to_owned(),
        model: model.to_owned(),
        query_for_otel: PROMPT.to_owned(),
        message_count: 1,
        response_content: ResponseContent::new(),
        pending_approvals: PendingApprovals::new(),
    };
    let setup = RequestSetup {
        query: PROMPT.to_owned(),
        chat_history: Vec::new(),
        streaming_agent: agent,
        config,
        completion_id: COMPLETION_ID.to_owned(),
        model_str: MODEL.to_owned(),
        created_timestamp: CREATED,
        chat_session_id: session(),
        has_client_tools: script.client_tools,
        request_id: run.to_string(),
        user_id: Some(USER.to_owned()),
        metadata_json: Some(serde_json::to_string(&metadata()).expect("metadata serializes")),
        tools_json: Vec::new(),
        rehydrated_skills: Vec::new(),
    };

    let (chunks, mut received) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(64);
    let collector = tokio::spawn(async move {
        let mut body = Vec::new();
        while let Some(chunk) = received.recv().await {
            body.extend_from_slice(&chunk.expect("the request-bound path sends no errors"));
        }
        body
    });
    let span = tracing::info_span!(parent: None, "agent.stream", run.id = %run);
    tokio::time::timeout(
        BODY_DEADLINE,
        execute_completion(
            setup,
            completion,
            DeliveryMode::Sse {
                chunk_tx: chunks,
                heartbeat_interval: QUIET,
            },
        )
        .instrument(span),
    )
    .await
    .expect("the request-bound path's body ended");
    let body = collector.await.expect("the collector does not panic");

    Rendered {
        body: normalise_body(&body, 1),
        spans: render_spans(&captured, run, script.ending, Side::Oracle),
    }
}

/// Runs the runtime path. A runtime serves the mock, and once the run's
/// `Started` event has been read, `sse_body` renders the rest of the
/// session's stream with no timeout windows and no heartbeat.
async fn runtime_backed(runtime: Arc<AgentRuntime>, script: Script, variant: Variant) -> Rendered {
    let captured = CapturedSpans::default();
    let _subscriber = captured.install();

    let (subscription, run) = attach_and_start(&runtime).await;
    let projector = SseProjector::new(script.stream_config(variant), script.turn(), run);
    let body = tokio::time::timeout(
        BODY_DEADLINE,
        sse_body(
            Arc::clone(&runtime),
            subscription,
            projector,
            Timeouts::default(),
        )
        .fold(Vec::new(), |mut body, chunk| async move {
            body.extend_from_slice(&chunk.expect("the body sends no errors"));
            body
        }),
    )
    .await
    .expect("the runtime path's body ended");
    span_closed(&captured).await;

    Rendered {
        body: normalise_body(&body, 0),
        spans: render_spans(&captured, run, script.ending, Side::Runtime),
    }
}

/// Renders only the runtime path's span. The test reads the session's
/// stream itself until the run ends, instead of handing it to `sse_body`.
async fn runtime_span(runtime: Arc<AgentRuntime>, script: Script) -> String {
    let captured = CapturedSpans::default();
    let _subscriber = captured.install();

    let (mut subscription, run) = attach_and_start(&runtime).await;
    loop {
        let item = tokio::time::timeout(BODY_DEADLINE, subscription.next())
            .await
            .expect("the run ended")
            .expect("the session's stream ended before the run did");
        if let aura::journal::JournalItem::Event(event) = item
            && event.run_id == Some(run)
            && event.ends_run()
        {
            break;
        }
    }
    span_closed(&captured).await;
    drop(subscription);
    render_spans(&captured, run, script.ending, Side::Runtime)
}

/// Attaches an observer that claims the session with presence, then starts
/// the run with an `agent.stream` span for the runtime to record `run.id`
/// on. Reads the session's stream up to the run's `Started` event, and
/// panics if the run ends, or the stream ends or skips ahead, before that.
async fn attach_and_start(runtime: &AgentRuntime) -> (Subscription, RunId) {
    let session = session();
    let mut subscription = runtime
        .attach(&session, Attach::claiming().present())
        .await
        .expect("the attach is admitted");
    let span = tracing::info_span!(parent: None, "agent.stream", run.id = tracing::field::Empty);
    let run = runtime
        .start(StartRun {
            target: RunTarget::Session(session),
            caller: Some(Caller {
                user: Some(USER.to_owned()),
                metadata: Some(metadata()),
            }),
            span,
            ..StartRun::new(PROMPT)
        })
        .await
        .expect("the start is admitted");
    loop {
        let item = subscription
            .next()
            .await
            .expect("the session's stream ended before the run started");
        let aura::journal::JournalItem::Event(event) = item else {
            panic!("the session's stream skipped ahead before the run started");
        };
        if event.run_id != Some(run) {
            continue;
        }
        match &event.payload {
            SessionEventPayload::Lifecycle(LifecycleEvent::Started { .. }) => {
                return (subscription, run);
            }
            SessionEventPayload::Lifecycle(ending) if ending.ends_run() => {
                panic!("the run ended before it started: {ending:?}")
            }
            _ => {}
        }
    }
}

/// Waits until the run's task closes its `agent.stream` span. The task
/// closes the span after it writes the run's ending.
async fn span_closed(captured: &CapturedSpans) {
    for _ in 0..10_000 {
        if captured.contains("agent.stream") {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the run's agent.stream span never closed");
}

fn serving(script: Script) -> Arc<AgentRuntime> {
    Arc::new(AgentRuntime::over_agents(
        Arc::new(vec![script.config()]),
        None,
        RuntimeStores::in_memory(),
        move |_, run| {
            Ok(AgentForRun {
                agent: Arc::new(script.agent(run)),
                attribution: script.attribution(),
            })
        },
    ))
}

/// Splits `body` into SSE frames and rewrites the two things that depend on
/// the clock rather than on the run:
///
/// - Heartbeat comments are dropped. The body must hold exactly
///   `heartbeats` of them.
/// - The `duration_ms` of each `aura.tool_complete` frame becomes
///   `<elapsed>`, because both paths measure it on the wall clock.
///
/// Every frame must end with a blank line, and each `aura.tool_complete`
/// frame must hold exactly one `duration_ms`. Anything else panics, so a
/// frame the rewrite did not expect fails the test instead of slipping
/// through.
fn normalise_body(body: &[u8], heartbeats: usize) -> String {
    let body = std::str::from_utf8(body).expect("the body is UTF-8");
    let frames: Vec<&str> = body
        .split_inclusive("\n\n")
        .inspect(|frame| {
            assert!(
                frame.ends_with("\n\n") && frame.len() > 2,
                "a frame that does not end in a blank line: {frame:?}"
            )
        })
        .collect();

    let seen = frames
        .iter()
        .filter(|frame| **frame == ": heartbeat\n\n")
        .count();
    assert_eq!(
        seen, heartbeats,
        "the body holds {seen} heartbeats where {heartbeats} were expected:\n{body}"
    );

    let tool_complete = event_frame(aura_events::event_names::TOOL_COMPLETE);
    frames
        .into_iter()
        .filter(|frame| *frame != ": heartbeat\n\n")
        .map(|frame| {
            if !frame.starts_with(&tool_complete) {
                return frame.to_owned();
            }
            const FIELD: &str = r#""duration_ms":"#;
            assert_eq!(
                frame.matches(FIELD).count(),
                1,
                "a tool_complete frame with other than one duration: {frame:?}"
            );
            let (head, rest) = frame.split_once(FIELD).expect("counted above");
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            assert!(digits > 0, "a duration that is not a number: {frame:?}");
            format!("{head}{FIELD}<elapsed>{}", &rest[digits..])
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Oracle,
    Runtime,
}

/// An attribute value as its type and its text. The text alone renders
/// `I64(1)` and `String("1")` the same, and `Debug` tells a static string
/// from an owned one, which neither path controls.
fn typed(value: &Value) -> String {
    let kind = match value {
        Value::Bool(_) => "bool",
        Value::I64(_) => "i64",
        Value::F64(_) => "f64",
        Value::String(_) => "string",
        Value::Array(Array::Bool(_)) => "bool[]",
        Value::Array(Array::I64(_)) => "i64[]",
        Value::Array(Array::F64(_)) => "f64[]",
        Value::Array(Array::String(_)) => "string[]",
        other => panic!("an attribute type this test does not know: {other:?}"),
    };
    format!("{kind} {value}")
}

/// Renders the captured span tree from `agent.stream` down as text, with one
/// line per span, per attribute and per `aura::logging` event. Span ids,
/// trace ids and timestamps are left out. Before dropping or rewriting
/// anything, it asserts what it expects to find:
///
/// - Exactly one `agent.stream` root.
/// - On every span, exactly one each of the attributes the tracing layer
///   adds: location (`code.*`), thread (`thread.id`, and at most one
///   `thread.name`) and busy/idle timing. These are dropped, because each
///   path creates its span at its own call site and on its own schedule.
/// - On the root, exactly one `run.id` and one `http.request_id`, both equal
///   to the run's id. Both become `<run>`, because each path's run has its
///   own id.
/// - No span dropped an event.
///
/// Span events are kept only when they come from `aura::logging`, the span
/// helpers both paths record through. A failed span's error event comes from
/// there. Other events are each path's own log lines.
///
/// The request-bound path records a parked run as complete. The runtime path
/// records `stream.termination = parked` with no status. For a script whose
/// run parks, the oracle's root is rewritten to the runtime's park ending,
/// after asserting that it ended ok with no termination attribute.
fn render_spans(captured: &CapturedSpans, run: RunId, ending: SpanEnding, side: Side) -> String {
    const DROPPED_ONCE: [&str; 6] = [
        "code.filepath",
        "code.namespace",
        "code.lineno",
        "thread.id",
        "busy_ns",
        "idle_ns",
    ];
    let spans = captured.spans();
    let ids: Vec<_> = spans
        .iter()
        .map(|span| span.span_context.span_id())
        .collect();
    let roots: Vec<usize> = (0..spans.len())
        .filter(|&i| spans[i].name == "agent.stream" && !ids.contains(&spans[i].parent_span_id))
        .collect();
    let [root] = roots.as_slice() else {
        panic!(
            "expected one agent.stream root, found {}: {:?}",
            roots.len(),
            spans.iter().map(|span| &span.name).collect::<Vec<_>>()
        );
    };

    let run = run.to_string();
    let mut out = String::new();
    let mut stack = vec![(*root, 0usize)];
    while let Some((index, depth)) = stack.pop() {
        let span = &spans[index];
        let indent = "  ".repeat(depth);
        assert_eq!(
            span.events.dropped_count, 0,
            "span {} dropped events, so the ones compared may be missing",
            span.name
        );
        for key in DROPPED_ONCE {
            let count = span
                .attributes
                .iter()
                .filter(|kv| kv.key.as_str() == key)
                .count();
            assert_eq!(count, 1, "span {} carries {count} {key}", span.name);
        }
        let thread_names = span
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str() == "thread.name")
            .count();
        assert!(
            thread_names <= 1,
            "span {} carries {thread_names} thread.name",
            span.name
        );

        let mut status = format!("{:?}", span.status);
        let mut attributes: Vec<(String, String)> = span
            .attributes
            .iter()
            .filter(|kv| {
                let key = kv.key.as_str();
                !DROPPED_ONCE.contains(&key) && key != "thread.name"
            })
            .map(|kv| (kv.key.as_str().to_owned(), typed(&kv.value)))
            .collect();
        if index == *root {
            for key in ["run.id", "http.request_id"] {
                let values: Vec<_> = attributes.iter_mut().filter(|(k, _)| k == key).collect();
                let [(_, value)] = values.as_slice() else {
                    panic!("the {side:?} root carries {} {key}", values.len());
                };
                assert_eq!(
                    *value,
                    typed(&Value::from(run.clone())),
                    "the {side:?} root's {key} is not its run"
                );
            }
            for (key, value) in &mut attributes {
                if key == "run.id" || key == "http.request_id" {
                    *value = "<run>".to_owned();
                }
            }
            // Only the runtime path has a park ending. The request-bound path
            // ends a parked run with status ok and no termination attribute.
            // The runtime's termination table records a park as status unset
            // with `stream.termination = parked`. Both paths record every
            // other ending the same way.
            if ending == SpanEnding::Parked && side == Side::Oracle {
                assert_eq!(status, "Ok", "the oracle ends a parked run ok");
                assert!(
                    !attributes
                        .iter()
                        .any(|(key, _)| key == "stream.termination"),
                    "the oracle records no termination for a parked run"
                );
                status = "Unset".to_owned();
                attributes.push((
                    "stream.termination".to_owned(),
                    typed(&Value::from("parked")),
                ));
            }
        }

        out.push_str(&format!(
            "{indent}span {} kind={:?} status={status}\n",
            span.name, span.span_kind
        ));
        for (key, value) in attributes {
            out.push_str(&format!("{indent}  attr {key} = {value}\n"));
        }
        for event in span.events.iter().filter(|event| {
            event
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "target" && kv.value.to_string() == "aura::logging")
        }) {
            out.push_str(&format!("{indent}  event {:?}\n", event.name));
            for kv in &event.attributes {
                out.push_str(&format!(
                    "{indent}    attr {} = {}\n",
                    kv.key.as_str(),
                    typed(&kv.value)
                ));
            }
        }

        let mut children: Vec<usize> = (0..spans.len())
            .filter(|&i| spans[i].parent_span_id == ids[index])
            .collect();
        children.sort_by(|&a, &b| {
            (spans[a].start_time, &spans[a].name).cmp(&(spans[b].start_time, &spans[b].name))
        });
        stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
    }
    out
}

/// Asserts that the oracle's body ends with `[DONE]` and holds every mark
/// the script lists, and that its span tree starts at `agent.stream`.
fn check_oracle(script: Script, variant: Variant, oracle: &Rendered) {
    let case = format!("{} [{}]", script.name, variant.label());
    assert!(
        oracle.body.ends_with("data: [DONE]\n\n"),
        "{case}: the oracle's body does not end the stream:\n{}",
        oracle.body
    );
    assert!(
        oracle.spans.starts_with("span agent.stream "),
        "{case}: the oracle's span tree:\n{}",
        oracle.spans
    );
    for mark in (script.covers)(variant) {
        match mark {
            Mark::Has(text) => assert!(
                oracle.body.contains(&text),
                "{case}: the oracle's body lacks {text:?}:\n{}",
                oracle.body
            ),
            Mark::Lacks(text) => assert!(
                !oracle.body.contains(&text),
                "{case}: the oracle's body holds {text:?}:\n{}",
                oracle.body
            ),
            Mark::Before(first, second) => {
                let at = |text: &str| {
                    oracle.body.find(text).unwrap_or_else(|| {
                        panic!("{case}: the oracle's body lacks {text:?}:\n{}", oracle.body)
                    })
                };
                assert!(
                    at(&first) < at(&second),
                    "{case}: {first:?} does not come before {second:?}:\n{}",
                    oracle.body
                );
            }
        }
    }
}

/// The first line where `oracle` and `served` differ, with both sides of it.
fn first_difference(oracle: &str, served: &str) -> String {
    let (mut oracle_lines, mut served_lines) = (oracle.lines(), served.lines());
    for line in 1.. {
        match (oracle_lines.next(), served_lines.next()) {
            (None, None) => break,
            (expected, got) if expected != got => {
                return format!(
                    "line {line}:\n    request-bound: {}\n    runtime:       {}",
                    expected.unwrap_or("<end>"),
                    got.unwrap_or("<end>")
                );
            }
            _ => {}
        }
    }
    "the lines agree; the bytes between them differ".to_owned()
}

/// Writes both sides of a mismatch to files a reader can diff, and returns
/// their paths.
fn write_both(case: &str, what: &str, oracle: &str, served: &str) -> String {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("sse_parity");
    std::fs::create_dir_all(&dir).expect("the parity output directory can be made");
    let oracle_path = dir.join(format!("{case}.{what}.request-bound"));
    let served_path = dir.join(format!("{case}.{what}.runtime"));
    std::fs::write(&oracle_path, oracle).expect("the request-bound side is written");
    std::fs::write(&served_path, served).expect("the runtime side is written");
    format!("{} and {}", oracle_path.display(), served_path.display())
}

/// Checks that every variant's oracle body holds what the script covers,
/// and that a second run renders the same. An oracle that changed from run
/// to run would fail the comparison for reasons that have nothing to do
/// with the runtime path.
async fn the_oracle_covers(script: Script) {
    for &variant in script.variants {
        let case = format!("{}.{}", script.name, variant.label());
        let oracle = request_bound(script, variant).await;
        check_oracle(script, variant, &oracle);
        let again = request_bound(script, variant).await;
        let failures = differences(&format!("{case}.again"), &oracle, &again);
        assert!(
            failures.is_empty(),
            "the oracle differs from itself:\n{}",
            failures.join("\n")
        );
    }
}

/// Compares the runtime path's span with the oracle's. The run's task
/// records the span, not the body, so this test builds no body.
async fn the_spans_agree(script: Script) {
    for &variant in script.variants {
        let case = format!("{}.{}", script.name, variant.label());
        let oracle = request_bound(script, variant).await;
        let runtime = serving(script);
        let spans = runtime_span(Arc::clone(&runtime), script).await;
        runtime.drain(Duration::from_secs(1)).await;
        if spans != oracle.spans {
            let written = write_both(&case, "spans", &oracle.spans, &spans);
            panic!(
                "{case}: the spans differ at {}\n  both written to {written}",
                first_difference(&oracle.spans, &spans)
            );
        }
    }
}

/// Lists what differs between the two sides of `case`, and writes each
/// differing pair to files.
fn differences(case: &str, oracle: &Rendered, served: &Rendered) -> Vec<String> {
    let mut failures = Vec::new();
    if served.body != oracle.body {
        let written = write_both(case, "sse", &oracle.body, &served.body);
        failures.push(format!(
            "{case}: the bodies differ at {}\n  both written to {written}",
            first_difference(&oracle.body, &served.body)
        ));
    }
    if served.spans != oracle.spans {
        let written = write_both(case, "spans", &oracle.spans, &served.spans);
        failures.push(format!(
            "{case}: the spans differ at {}\n  both written to {written}",
            first_difference(&oracle.spans, &served.spans)
        ));
    }
    failures
}

async fn the_paths_agree(script: Script) {
    let mut failures = Vec::new();
    for &variant in script.variants {
        let case = format!("{}.{}", script.name, variant.label());
        let oracle = request_bound(script, variant).await;
        check_oracle(script, variant, &oracle);

        let runtime = serving(script);
        let served = AssertUnwindSafe(runtime_backed(Arc::clone(&runtime), script, variant))
            .catch_unwind()
            .await;
        runtime.drain(Duration::from_secs(1)).await;
        let served = match served {
            Ok(served) => served,
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("no message");
                failures.push(format!("{case}: the runtime path panicked: {message}"));
                continue;
            }
        };
        failures.extend(differences(&case, &oracle, &served));
    }
    assert!(
        failures.is_empty(),
        "{} of {} variants of {} differ between the paths:\n{}",
        failures.len(),
        script.variants.len(),
        script.name,
        failures.join("\n")
    );
}

macro_rules! parity {
    ($($script:ident),* $(,)?) => {
        /// The oracle alone: each script's body holds what it covers.
        mod original_path {
            $(
                #[tokio::test(start_paused = true)]
                async fn $script() {
                    super::the_oracle_covers(super::scripts::$script()).await;
                }
            )*
        }

        /// The runtime path's span compared with the oracle's, without
        /// building a body.
        mod runtime_span {
            $(
                #[tokio::test(start_paused = true)]
                async fn $script() {
                    super::the_spans_agree(super::scripts::$script()).await;
                }
            )*
        }

        /// Both paths, compared as bytes and as spans.
        mod both_paths {
            $(
                #[tokio::test(start_paused = true)]
                async fn $script() {
                    super::the_paths_agree(super::scripts::$script()).await;
                }
            )*
        }
    };
}

parity!(
    text_and_final,
    tool_turn,
    tool_outcomes,
    unparsed_tool_arguments,
    fallback_tool_call,
    reasoning_then_text,
    final_with_cache_split,
    provider_error,
    context_overflow,
    passthrough_tool,
    max_tokens_length,
    approvals,
    mcp_status_at_start,
    scratchpad_tail,
    scratchpad_tool_hidden,
    orchestration_events,
    orchestration_direct_answer,
    orchestration_clarification,
    orchestration_parks,
    orchestration_event_ahead_of_content,
);

/// Checks the comparison and its rewrites on inputs whose differences are
/// known.
mod controls {
    use super::*;

    #[test]
    fn an_attribute_renders_with_its_type() {
        assert_ne!(typed(&Value::I64(1)), typed(&Value::from("1")));
        assert_ne!(typed(&Value::Bool(true)), typed(&Value::from("true")));
        assert_eq!(
            typed(&Value::from("1")),
            typed(&Value::String(StringValue::from("1".to_owned()))),
            "a static and an owned string render the same"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_changed_byte_is_reported_at_its_line() {
        let oracle = request_bound(scripts::text_and_final(), PLAIN[0]).await;
        let line = oracle
            .body
            .lines()
            .position(|line| line.contains(r#""content":"Hello""#))
            .expect("the oracle carries the first text chunk")
            + 1;
        let changed = Rendered {
            body: oracle
                .body
                .replacen(r#""content":"Hello""#, r#""content":"Jello""#, 1),
            spans: oracle.spans.clone(),
        };

        let failures = differences("control.byte", &oracle, &changed);
        let [failure] = failures.as_slice() else {
            panic!("one byte is one difference: {failures:?}");
        };
        assert!(
            failure.contains("the bodies differ") && failure.contains(&format!("line {line}:")),
            "{failure}"
        );
        assert!(failure.contains("Jello"), "{failure}");
    }

    /// A changed value is a difference, and so is the same value with
    /// another type.
    #[tokio::test(start_paused = true)]
    async fn a_changed_attribute_is_reported_in_the_spans() {
        let oracle = request_bound(scripts::text_and_final(), PLAIN[0]).await;
        let original = format!("attr message_count = {}", typed(&Value::I64(1)));
        for replacement in [
            format!("attr message_count = {}", typed(&Value::I64(2))),
            format!("attr message_count = {}", typed(&Value::from("1"))),
        ] {
            assert!(oracle.spans.contains(&original), "{}", oracle.spans);
            let changed = Rendered {
                body: oracle.body.clone(),
                spans: oracle.spans.replacen(&original, &replacement, 1),
            };

            let failures = differences("control.attribute", &oracle, &changed);
            let [failure] = failures.as_slice() else {
                panic!("one attribute is one difference: {failures:?}");
            };
            assert!(
                failure.contains("the spans differ") && failure.contains(&replacement),
                "{failure}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "heartbeats where 1 were expected")]
    fn an_unexpected_heartbeat_count_is_refused() {
        normalise_body(b": heartbeat\n\n: heartbeat\n\ndata: [DONE]\n\n", 1);
    }

    #[test]
    #[should_panic(expected = "does not end in a blank line")]
    fn a_frame_cut_short_is_refused() {
        normalise_body(b"data: [DONE]\n", 0);
    }

    #[test]
    #[should_panic(expected = "other than one duration")]
    fn a_tool_complete_frame_without_its_duration_is_refused() {
        normalise_body(
            b"event: aura.tool_complete\ndata: {\"tool_id\":\"t\"}\n\n",
            0,
        );
    }

    #[test]
    fn only_the_tool_complete_duration_is_rewritten() {
        let body = concat!(
            ": heartbeat\n\n",
            "event: aura.tool_complete\ndata: {\"duration_ms\":17,\"success\":true}\n\n",
            "event: aura.orchestrator.task_completed\ndata: {\"duration_ms\":250}\n\n",
        );
        assert_eq!(
            normalise_body(body.as_bytes(), 1),
            concat!(
                "event: aura.tool_complete\ndata: {\"duration_ms\":<elapsed>,\"success\":true}\n\n",
                "event: aura.orchestrator.task_completed\ndata: {\"duration_ms\":250}\n\n",
            )
        );
    }
}
