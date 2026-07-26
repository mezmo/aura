//! Bounded polling via the `wait_for` tool.

use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::time::Duration;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::mcp::McpManager;
use crate::mcp_tool_execution::execute_mcp_tool;

/// Hard ceiling on any single wait.
pub const MAX_WAIT_HARD_CEILING_SECS: u64 = 300;

/// Default poll cadence.
pub const POLL_DEFAULT_SECS: u64 = 2;

/// Default wait bound.
pub const MAX_WAIT_DEFAULT_SECS: u64 = 120;

/// Largest probe output a single sample may return.
pub const MAX_OBSERVATION_BYTES: usize = 256 * 1024;

// ============================================================================
// Wire types (the JSON surface the model authors)
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitForArgs {
    pub probe: ProbeSpec,
    pub until: UntilSpec,
    #[serde(default)]
    pub poll_sec: Option<u64>,
    #[serde(default)]
    pub max_wait_sec: Option<u64>,
}

/// Wire form of the probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSpec {
    pub tool: String,
    pub args: serde_json::Value,
}

/// Wire form of the stop condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UntilSpec {
    Matches(String),
    NotMatches(String),
    QuietForSec(u64),
}

// ============================================================================
// Domain types (parsed, invariant-bearing)
// ============================================================================

/// Non-empty name of the MCP tool sampled as the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeToolName(String);

impl ProbeToolName {
    pub fn new(raw: &str) -> Result<Self, WaitForCallError> {
        if raw.is_empty() {
            return Err(WaitForCallError::EmptyProbeTool);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProbeToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An MCP tool name paired with object-shaped arguments.
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    tool: ProbeToolName,
    args: serde_json::Map<String, serde_json::Value>,
}

impl ProbeRequest {
    pub fn tool(&self) -> &ProbeToolName {
        &self.tool
    }

    pub fn args(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.args
    }
}

impl TryFrom<ProbeSpec> for ProbeRequest {
    type Error = WaitForCallError;

    fn try_from(spec: ProbeSpec) -> Result<Self, Self::Error> {
        let tool = ProbeToolName::new(&spec.tool)?;
        let serde_json::Value::Object(args) = spec.args else {
            return Err(WaitForCallError::ProbeArgsNotObject);
        };
        Ok(Self { tool, args })
    }
}

#[derive(Debug, Clone)]
pub struct ProbePattern(regex::Regex);

impl ProbePattern {
    pub fn new(pattern: &str) -> Result<Self, WaitForCallError> {
        Ok(Self(regex::Regex::new(pattern)?))
    }

    pub fn as_regex(&self) -> &regex::Regex {
        &self.0
    }
}

/// Quiescence window: probe output unchanged for this many seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietWindow(NonZeroU64);

impl QuietWindow {
    pub fn from_secs(secs: u64) -> Result<Self, WaitForCallError> {
        NonZeroU64::new(secs)
            .map(Self)
            .ok_or(WaitForCallError::ZeroQuietWindow)
    }

    pub fn as_secs(&self) -> u64 {
        self.0.get()
    }

    pub fn as_duration(&self) -> Duration {
        Duration::from_secs(self.0.get())
    }
}

#[derive(Debug, Clone)]
pub enum StopCondition {
    Matches(ProbePattern),
    NotMatches(ProbePattern),
    QuietFor(QuietWindow),
}

impl TryFrom<UntilSpec> for StopCondition {
    type Error = WaitForCallError;

    fn try_from(spec: UntilSpec) -> Result<Self, Self::Error> {
        match spec {
            UntilSpec::Matches(pattern) => Ok(Self::Matches(ProbePattern::new(&pattern)?)),
            UntilSpec::NotMatches(pattern) => Ok(Self::NotMatches(ProbePattern::new(&pattern)?)),
            UntilSpec::QuietForSec(secs) => Ok(Self::QuietFor(QuietWindow::from_secs(secs)?)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PollInterval(NonZeroU64);

impl PollInterval {
    pub fn from_secs(secs: u64) -> Result<Self, WaitForCallError> {
        NonZeroU64::new(secs)
            .map(Self)
            .ok_or(WaitForCallError::ZeroPollInterval)
    }

    pub fn as_secs(&self) -> u64 {
        self.0.get()
    }

    pub fn as_duration(&self) -> Duration {
        Duration::from_secs(self.0.get())
    }
}

impl Default for PollInterval {
    fn default() -> Self {
        Self(
            const {
                match NonZeroU64::new(POLL_DEFAULT_SECS) {
                    Some(secs) => secs,
                    None => unreachable!(),
                }
            },
        )
    }
}

/// Total wait bound in seconds, never above [`MAX_WAIT_HARD_CEILING_SECS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WaitBound(NonZeroU64);

impl WaitBound {
    pub fn from_secs(secs: u64) -> Result<Self, WaitForCallError> {
        NonZeroU64::new(secs.min(MAX_WAIT_HARD_CEILING_SECS))
            .map(Self)
            .ok_or(WaitForCallError::ZeroWaitBound)
    }

    pub fn as_secs(&self) -> u64 {
        self.0.get()
    }

    pub fn as_duration(&self) -> Duration {
        Duration::from_secs(self.0.get())
    }
}

impl Default for WaitBound {
    fn default() -> Self {
        Self(
            const {
                match NonZeroU64::new(MAX_WAIT_DEFAULT_SECS) {
                    Some(secs) => secs,
                    None => unreachable!(),
                }
            },
        )
    }
}

/// A poll interval paired with a strictly larger wait bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitBudget {
    poll: PollInterval,
    bound: WaitBound,
}

impl WaitBudget {
    pub fn new(poll: PollInterval, bound: WaitBound) -> Result<Self, WaitForCallError> {
        if poll.as_secs() >= bound.as_secs() {
            return Err(WaitForCallError::PollExceedsBound {
                poll_secs: poll.as_secs(),
                bound_secs: bound.as_secs(),
                ceiling: MAX_WAIT_HARD_CEILING_SECS,
            });
        }
        Ok(Self { poll, bound })
    }

    pub fn poll(&self) -> PollInterval {
        self.poll
    }

    pub fn bound(&self) -> WaitBound {
        self.bound
    }
}

/// A fully parsed `wait_for` call.
#[derive(Debug, Clone)]
pub struct WaitForCall {
    probe: ProbeRequest,
    condition: StopCondition,
    budget: WaitBudget,
}

impl WaitForCall {
    /// Apply defaults for omitted fields, then check every cross-field rule.
    pub fn parse(args: WaitForArgs) -> Result<Self, WaitForCallError> {
        let probe = ProbeRequest::try_from(args.probe)?;
        let condition = StopCondition::try_from(args.until)?;
        let poll = args
            .poll_sec
            .map(PollInterval::from_secs)
            .transpose()?
            .unwrap_or_default();
        let bound = args
            .max_wait_sec
            .map(WaitBound::from_secs)
            .transpose()?
            .unwrap_or_default();
        let budget = WaitBudget::new(poll, bound)?;
        if let StopCondition::QuietFor(window) = &condition
            && window.as_secs() > bound.as_secs()
        {
            return Err(WaitForCallError::QuietWindowExceedsBound {
                quiet_secs: window.as_secs(),
                bound_secs: bound.as_secs(),
                ceiling: MAX_WAIT_HARD_CEILING_SECS,
            });
        }
        Ok(Self {
            probe,
            condition,
            budget,
        })
    }

    pub fn probe(&self) -> &ProbeRequest {
        &self.probe
    }

    pub fn condition(&self) -> &StopCondition {
        &self.condition
    }

    pub fn budget(&self) -> WaitBudget {
        self.budget
    }
}

// ============================================================================
// Condition evaluation
// ============================================================================

/// A stop condition with its per-sample evaluation state.
enum ConditionEvaluator {
    Matches(ProbePattern),
    NotMatches(ProbePattern),
    QuietFor {
        window: QuietWindow,
        observed: Option<QuietState>,
    },
}

struct QuietState {
    last: String,
    unchanged_since: Duration,
}

impl ConditionEvaluator {
    fn new(condition: StopCondition) -> Self {
        match condition {
            StopCondition::Matches(pattern) => Self::Matches(pattern),
            StopCondition::NotMatches(pattern) => Self::NotMatches(pattern),
            StopCondition::QuietFor(window) => Self::QuietFor {
                window,
                observed: None,
            },
        }
    }

    /// Feed one sample; `Some` means the condition held on this sample.
    fn observe(&mut self, observation: &str, elapsed: Duration) -> Option<StopReason> {
        match self {
            Self::Matches(pattern) => pattern
                .as_regex()
                .is_match(observation)
                .then_some(StopReason::Matched),
            Self::NotMatches(pattern) => {
                (!pattern.as_regex().is_match(observation)).then_some(StopReason::Matched)
            }
            Self::QuietFor { window, observed } => match observed {
                None => {
                    *observed = Some(QuietState {
                        last: observation.to_owned(),
                        unchanged_since: elapsed,
                    });
                    None
                }
                Some(state) if state.last != observation => {
                    state.last = observation.to_owned();
                    state.unchanged_since = elapsed;
                    None
                }
                Some(state) => (elapsed.saturating_sub(state.unchanged_since)
                    >= window.as_duration())
                .then_some(StopReason::Settled),
            },
        }
    }
}

// ============================================================================
// Outcome types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StopReason {
    /// The predicate condition held on a sample.
    Matched,
    /// The quiescence condition held: output unchanged for the window.
    Settled,
    /// The bound elapsed with the condition never holding.
    Timeout,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::Matched => write!(f, "matched"),
            StopReason::Settled => write!(f, "settled"),
            StopReason::Timeout => write!(f, "timeout"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WaitForOutput {
    pub reason: StopReason,
    pub last_observation: String,
    pub elapsed_sec: u64,
    pub samples: NonZeroU32,
    pub effective_max_wait_sec: u64,
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum WaitForCallError {
    #[error("probe.tool must be a non-empty tool name")]
    EmptyProbeTool,
    #[error("probe.args must be a JSON object")]
    ProbeArgsNotObject,
    #[error("condition regex is invalid: {0}")]
    InvalidPattern(#[from] regex::Error),
    #[error("until.quiet_for_sec must be at least 1")]
    ZeroQuietWindow,
    #[error("poll_sec must be at least 1")]
    ZeroPollInterval,
    #[error("max_wait_sec must be at least 1")]
    ZeroWaitBound,
    #[error(
        "poll_sec {poll_secs}s must be strictly less than max_wait_sec {bound_secs}s; the loop could never take a second sample inside the bound (max_wait_sec is capped at {ceiling}s)"
    )]
    PollExceedsBound {
        poll_secs: u64,
        bound_secs: u64,
        ceiling: u64,
    },
    #[error(
        "quiet_for_sec {quiet_secs}s exceeds max_wait_sec {bound_secs}s; the condition could never hold (max_wait_sec is capped at {ceiling}s)"
    )]
    QuietWindowExceedsBound {
        quiet_secs: u64,
        bound_secs: u64,
        ceiling: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeSampleError {
    #[error("tool is not exposed by any connected MCP server")]
    UnknownTool,
    #[error(transparent)]
    Call(#[from] rig::tool::ToolError),
}

#[derive(Debug, thiserror::Error)]
pub enum WaitForError {
    #[error(transparent)]
    Call(#[from] WaitForCallError),
    #[error("probe tool '{tool}' is not exposed by any connected MCP server")]
    UnknownProbeTool { tool: String },
    #[error("probe tool '{tool}' failed on sample {sample}: {source}")]
    ProbeFailed {
        tool: String,
        sample: NonZeroU32,
        #[source]
        source: rig::tool::ToolError,
    },
    #[error(
        "probe tool '{tool}' returned nothing within max_wait_sec {bound_secs}s; the probe itself is hanging, so no observation is available"
    )]
    ProbeTimedOut { tool: String, bound_secs: u64 },
    #[error(
        "probe tool '{tool}' returned {bytes} bytes on sample {sample}, over the {limit}-byte cap; narrow the probe so it returns less"
    )]
    ObservationTooLarge {
        tool: String,
        sample: NonZeroU32,
        bytes: usize,
        limit: usize,
    },
}

// ============================================================================
// Seams: probe dispatch and sleep
// ============================================================================

#[async_trait::async_trait]
pub trait ProbeDispatcher: Send + Sync {
    async fn sample(&self, probe: &ProbeRequest) -> Result<String, ProbeSampleError>;
}

struct McpProbeDispatcher {
    mcp: Arc<McpManager>,
}

impl McpProbeDispatcher {
    /// Find the client of the server exposing `tool`.
    fn resolve(&self, tool: &str) -> Option<&crate::mcp_streamable_http::McpClient> {
        let manager = &self.mcp;
        [
            (&manager.streamable_tools, &manager.streamable_clients),
            (&manager.sse_tools, &manager.sse_clients),
            (&manager.stdio_tools, &manager.stdio_clients),
        ]
        .into_iter()
        .find_map(|(tools, clients)| {
            tools
                .iter()
                .find(|(_, server_tools)| server_tools.iter().any(|t| t.name == tool))
                .and_then(|(server_name, _)| clients.get(server_name))
        })
    }
}

#[async_trait::async_trait]
impl ProbeDispatcher for McpProbeDispatcher {
    async fn sample(&self, probe: &ProbeRequest) -> Result<String, ProbeSampleError> {
        let tool_name = probe.tool().as_str();
        let client = self
            .resolve(tool_name)
            .ok_or(ProbeSampleError::UnknownTool)?;
        let args = serde_json::Value::Object(probe.args().clone());
        Ok(execute_mcp_tool(client, tool_name, args).await?)
    }
}

#[async_trait::async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

pub struct TokioSleeper;

#[async_trait::async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

// ============================================================================
// The tool
// ============================================================================

/// Native orchestration tool exposed to workers as `wait_for`.
pub struct WaitForTool {
    dispatcher: Arc<dyn ProbeDispatcher>,
    sleeper: Arc<dyn Sleeper>,
}

impl WaitForTool {
    pub fn new(mcp: Arc<McpManager>) -> Self {
        Self::with_sleeper(mcp, Arc::new(TokioSleeper))
    }

    pub fn with_sleeper(mcp: Arc<McpManager>, sleeper: Arc<dyn Sleeper>) -> Self {
        Self {
            dispatcher: Arc::new(McpProbeDispatcher { mcp }),
            sleeper,
        }
    }

    #[cfg(test)]
    fn with_seams(dispatcher: Arc<dyn ProbeDispatcher>, sleeper: Arc<dyn Sleeper>) -> Self {
        Self {
            dispatcher,
            sleeper,
        }
    }

    /// The poll loop: sample immediately, then sleep the poll interval between
    /// samples, trimming the final sleep so the loop wakes at the bound. Each
    /// sample is raced against the remaining budget and evaluated before the
    /// bound check, so a sample completing at the bound can still match or
    /// settle; a sample that outruns its budget ends the wait — Timeout with
    /// the previous observation, ProbeTimedOut with none. The bound holds even
    /// if the probe hangs.
    async fn wait(&self, args: WaitForArgs) -> Result<WaitForOutput, WaitForError> {
        let span = tracing::Span::current();
        span.record("orchestration.probe_tool", args.probe.tool.as_str());
        let condition_verbatim = serde_json::to_string(&args.until)
            .expect("UntilSpec holds only strings and integers, which always serialize");
        span.record("orchestration.wait_condition", condition_verbatim.as_str());

        let call = WaitForCall::parse(args)?;
        let poll = call.budget().poll().as_duration();
        let bound = call.budget().bound().as_duration();
        let mut evaluator = ConditionEvaluator::new(call.condition().clone());

        let tool = || call.probe().tool().as_str().to_owned();
        let start = tokio::time::Instant::now();
        let mut samples: u32 = 0;
        let mut last: Option<String> = None;

        let verdict = loop {
            let remaining = bound.saturating_sub(start.elapsed());
            let sampled = tokio::time::timeout(remaining, self.dispatcher.sample(call.probe()));
            let observation = match sampled.await {
                Ok(Ok(observation)) => observation,
                Ok(Err(error)) => {
                    samples += 1;
                    let sample = NonZeroU32::new(samples).expect("just incremented");
                    break Err(match error {
                        ProbeSampleError::UnknownTool => {
                            WaitForError::UnknownProbeTool { tool: tool() }
                        }
                        ProbeSampleError::Call(source) => WaitForError::ProbeFailed {
                            tool: tool(),
                            sample,
                            source,
                        },
                    });
                }
                // The probe outran the budget. An earlier sample still stands
                // as the result; with none, there is nothing to report back.
                Err(_) => match last {
                    Some(observation) => break Ok((StopReason::Timeout, observation, samples)),
                    None => {
                        break Err(WaitForError::ProbeTimedOut {
                            tool: tool(),
                            bound_secs: call.budget().bound().as_secs(),
                        });
                    }
                },
            };
            samples += 1;
            let elapsed = start.elapsed();

            if observation.len() > MAX_OBSERVATION_BYTES {
                break Err(WaitForError::ObservationTooLarge {
                    tool: tool(),
                    sample: NonZeroU32::new(samples).expect("just incremented"),
                    bytes: observation.len(),
                    limit: MAX_OBSERVATION_BYTES,
                });
            }

            match evaluator.observe(&observation, elapsed) {
                Some(reason) => break Ok((reason, observation, samples)),
                None if elapsed >= bound => break Ok((StopReason::Timeout, observation, samples)),
                None => {
                    last = Some(observation);
                    self.sleeper.sleep(poll.min(bound - elapsed)).await;
                }
            }
        };

        let elapsed = start.elapsed();
        span.record("orchestration.poll_count", samples);
        span.record("orchestration.elapsed_ms", elapsed.as_millis() as u64);
        let (reason, last_observation, samples) = verdict?;
        span.record("orchestration.stop_reason", reason.to_string().as_str());
        Ok(WaitForOutput {
            reason,
            last_observation,
            elapsed_sec: elapsed.as_secs(),
            samples: NonZeroU32::new(samples).expect("a verdict always follows a sample"),
            effective_max_wait_sec: call.budget().bound().as_secs(),
        })
    }
}

impl WaitForTool {
    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: WaitForTool::NAME.to_string(),
            description: "Poll an MCP tool repeatedly until a stop condition you supply holds, \
                instead of sleeping blind. Give a probe (an MCP-provided tool, with its \
                arguments), a stop condition (a regex the output must match or stop \
                matching, or a quiet period after which unchanged output counts as settled), \
                and optional poll/bound seconds. Returns the stop reason, the last probe \
                output, elapsed seconds, and the sample count. A timeout is a normal result \
                carrying the last observation, not an error."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "probe": {
                        "type": "object",
                        "description": "The tool to sample on every poll.",
                        "properties": {
                            "tool": {
                                "type": "string",
                                "description": "Name of an MCP-provided tool. Native tools cannot be probed."
                            },
                            "args": {
                                "type": "object",
                                "description": "Arguments passed to the probe tool on every poll."
                            }
                        },
                        "required": ["tool", "args"],
                        "additionalProperties": false
                    },
                    "until": {
                        "type": "object",
                        "description": "Stop condition. Provide exactly ONE of the three keys.",
                        "oneOf": [
                            {
                                "properties": {
                                    "matches": {
                                        "type": "string",
                                        "description": "Regex; stop as soon as the probe output matches."
                                    }
                                },
                                "required": ["matches"],
                                "additionalProperties": false
                            },
                            {
                                "properties": {
                                    "not_matches": {
                                        "type": "string",
                                        "description": "Regex; stop as soon as the probe output no longer matches."
                                    }
                                },
                                "required": ["not_matches"],
                                "additionalProperties": false
                            },
                            {
                                "properties": {
                                    "quiet_for_sec": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "description": "Stop when the probe output is unchanged for this many consecutive seconds."
                                    }
                                },
                                "required": ["quiet_for_sec"],
                                "additionalProperties": false
                            }
                        ]
                    },
                    "poll_sec": {
                        "type": "integer",
                        "minimum": 1,
                        "default": POLL_DEFAULT_SECS,
                        "description": "Seconds between probe samples."
                    },
                    "max_wait_sec": {
                        "type": "integer",
                        "minimum": 1,
                        "default": MAX_WAIT_DEFAULT_SECS,
                        "description": format!(
                            "Total wait bound in seconds; values above {MAX_WAIT_HARD_CEILING_SECS} are clamped to {MAX_WAIT_HARD_CEILING_SECS}. The bound actually enforced is reported back as effective_max_wait_sec."
                        )
                    }
                },
                "required": ["probe", "until"],
                "additionalProperties": false
            }),
        }
    }
}

impl Tool for WaitForTool {
    const NAME: &'static str = "wait_for";

    type Error = WaitForError;
    type Args = WaitForArgs;
    type Output = WaitForOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        Self::tool_definition()
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let span = tracing::info_span!(
            "orchestration.wait_for",
            orchestration.probe_tool = tracing::field::Empty,
            orchestration.wait_condition = tracing::field::Empty,
            orchestration.poll_count = tracing::field::Empty,
            orchestration.stop_reason = tracing::field::Empty,
            orchestration.elapsed_ms = tracing::field::Empty,
        );
        self.wait(args).instrument(span).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Yields the queued outputs in order, then repeats the last forever.
    struct ScriptedProbe {
        outputs: Mutex<Vec<String>>,
        cursor: Mutex<usize>,
    }

    impl ScriptedProbe {
        fn new(outputs: &[&str]) -> Arc<Self> {
            assert!(
                !outputs.is_empty(),
                "scripted probe needs at least one output"
            );
            Arc::new(Self {
                outputs: Mutex::new(outputs.iter().map(|s| (*s).to_owned()).collect()),
                cursor: Mutex::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl ProbeDispatcher for ScriptedProbe {
        async fn sample(&self, _probe: &ProbeRequest) -> Result<String, ProbeSampleError> {
            let outputs = self.outputs.lock().unwrap();
            let mut cursor = self.cursor.lock().unwrap();
            let output = outputs[(*cursor).min(outputs.len() - 1)].clone();
            *cursor += 1;
            Ok(output)
        }
    }

    /// Succeeds until the given sample index, then fails the probe call.
    struct FailsOnSample {
        fail_on: u32,
        seen: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl ProbeDispatcher for FailsOnSample {
        async fn sample(&self, _probe: &ProbeRequest) -> Result<String, ProbeSampleError> {
            let mut seen = self.seen.lock().unwrap();
            *seen += 1;
            if *seen >= self.fail_on {
                return Err(ProbeSampleError::Call(rig::tool::ToolError::ToolCallError(
                    "probe transport dropped".into(),
                )));
            }
            Ok("still working".to_owned())
        }
    }

    /// Answers `answers` times, then hangs.
    struct HangsAfterSample {
        answers: Mutex<usize>,
    }

    impl HangsAfterSample {
        fn new(answers: usize) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers),
            })
        }
    }

    #[async_trait::async_trait]
    impl ProbeDispatcher for HangsAfterSample {
        async fn sample(&self, _probe: &ProbeRequest) -> Result<String, ProbeSampleError> {
            {
                let mut remaining = self.answers.lock().unwrap();
                if *remaining > 0 {
                    *remaining -= 1;
                    return Ok("working".to_owned());
                }
            }
            std::future::pending().await
        }
    }

    struct NoSuchTool;

    #[async_trait::async_trait]
    impl ProbeDispatcher for NoSuchTool {
        async fn sample(&self, _probe: &ProbeRequest) -> Result<String, ProbeSampleError> {
            Err(ProbeSampleError::UnknownTool)
        }
    }

    /// Records each requested interval and advances the tokio clock by it.
    struct AdvancingSleeper {
        requested: Mutex<Vec<Duration>>,
    }

    impl AdvancingSleeper {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requested: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl Sleeper for AdvancingSleeper {
        async fn sleep(&self, duration: Duration) {
            self.requested.lock().unwrap().push(duration);
            tokio::time::advance(duration).await;
        }
    }

    fn args(json: serde_json::Value) -> WaitForArgs {
        serde_json::from_value(json).expect("test args deserialize")
    }

    fn tool(probe: Arc<dyn ProbeDispatcher>) -> (WaitForTool, Arc<AdvancingSleeper>) {
        let sleeper = AdvancingSleeper::new();
        (
            WaitForTool::with_seams(probe, Arc::clone(&sleeper) as Arc<dyn Sleeper>),
            sleeper,
        )
    }

    fn probe_json() -> serde_json::Value {
        serde_json::json!({ "tool": "sample_probe", "args": {} })
    }

    #[tokio::test(start_paused = true)]
    async fn matched_on_first_sample() {
        let (tool, sleeper) = tool(ScriptedProbe::new(&["build finished ok"]));
        let output = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "matches": "finished" },
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Matched);
        assert_eq!(output.last_observation, "build finished ok");
        assert_eq!(output.samples.get(), 1);
        assert_eq!(output.elapsed_sec, 0);
        assert_eq!(output.effective_max_wait_sec, MAX_WAIT_DEFAULT_SECS);
        assert!(sleeper.requested.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn matched_after_n_polls() {
        let (tool, sleeper) = tool(ScriptedProbe::new(&[
            "running", "running", "running", "done",
        ]));
        let output = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "matches": "done" },
                "poll_sec": 3,
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Matched);
        assert_eq!(output.last_observation, "done");
        assert_eq!(output.samples.get(), 4);
        assert_eq!(output.elapsed_sec, 9);
        assert_eq!(
            *sleeper.requested.lock().unwrap(),
            vec![Duration::from_secs(3); 3]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn not_matches_is_matched_when_pattern_disappears() {
        let (tool, _) = tool(ScriptedProbe::new(&[
            "still running",
            "still running",
            "idle",
        ]));
        let output = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "not_matches": "running" },
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Matched);
        assert_eq!(output.samples.get(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn settled_via_quiet_window() {
        // Changes at samples 1->2, then stays constant; quiet 4s at poll 2s
        // means settled on the sample 4s after the last change.
        let (tool, _) = tool(ScriptedProbe::new(&["a", "b", "b", "b", "b"]));
        let output = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "quiet_for_sec": 4 },
                "poll_sec": 2,
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Settled);
        assert_eq!(output.last_observation, "b");
        assert_eq!(output.samples.get(), 4);
        assert_eq!(output.elapsed_sec, 6);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_returns_last_observation_on_success_track() {
        let (tool, sleeper) = tool(ScriptedProbe::new(&["never the droid"]));
        let output = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "matches": "ready_token" },
                "poll_sec": 2,
                "max_wait_sec": 5,
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Timeout);
        assert_eq!(output.last_observation, "never the droid");
        assert_eq!(output.samples.get(), 4);
        assert_eq!(output.elapsed_sec, 5);
        assert_eq!(
            *sleeper.requested.lock().unwrap(),
            vec![
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(1)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn clamp_above_ceiling_reported_in_effective_bound() {
        let (tool, _) = tool(ScriptedProbe::new(&["quiet output"]));
        let output = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "matches": "never_present" },
                "poll_sec": 250,
                "max_wait_sec": 400,
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Timeout);
        assert_eq!(output.effective_max_wait_sec, MAX_WAIT_HARD_CEILING_SECS);
        assert_eq!(output.elapsed_sec, MAX_WAIT_HARD_CEILING_SECS);
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_window_equal_to_bound_settles_at_the_bound() {
        // NOTE: Settled at the bound relies on ScriptedProbe::sample completing
        // synchronously — tokio::time::timeout(Duration::ZERO, ..) polls the
        // inner future before checking the sleep, and a ready future wins the
        // race. A real async MCP probe is Pending on first poll, so the
        // zero-duration timeout fires and the result is Timeout, not Settled.
        let (tool, _) = tool(ScriptedProbe::new(&["constant"]));
        let output = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "quiet_for_sec": 6 },
                "poll_sec": 2,
                "max_wait_sec": 6,
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Settled);
        assert_eq!(output.elapsed_sec, 6);
    }

    #[tokio::test(start_paused = true)]
    async fn probe_call_failure_carries_tool_name_and_sample_index() {
        let (tool, _) = tool(Arc::new(FailsOnSample {
            fail_on: 2,
            seen: Mutex::new(0),
        }));
        let error = tool
            .call(args(serde_json::json!({
                "probe": probe_json(),
                "until": { "matches": "never_present" },
            })))
            .await
            .unwrap_err();
        let WaitForError::ProbeFailed {
            tool,
            sample,
            source,
        } = error
        else {
            panic!("expected ProbeFailed, got {error:?}");
        };
        assert_eq!(tool, "sample_probe");
        assert_eq!(sample.get(), 2);
        assert!(source.to_string().contains("probe transport dropped"));
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_probe_tool_is_an_error() {
        let (tool, _) = tool(Arc::new(NoSuchTool));
        let error = tool
            .call(args(serde_json::json!({
                "probe": { "tool": "ghost_tool", "args": {} },
                "until": { "matches": "anything" },
            })))
            .await
            .unwrap_err();
        let WaitForError::UnknownProbeTool { tool } = error else {
            panic!("expected UnknownProbeTool, got {error:?}");
        };
        assert_eq!(tool, "ghost_tool");
    }

    #[test]
    fn parse_rejects_every_invalid_call() {
        use WaitForCallError as E;
        type Expected = fn(&E) -> bool;

        let cases: Vec<(serde_json::Value, Expected)> = vec![
            (
                serde_json::json!({
                    "probe": { "tool": "", "args": {} },
                    "until": { "matches": "x" },
                }),
                |e| matches!(e, E::EmptyProbeTool),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": [1, 2] },
                    "until": { "matches": "x" },
                }),
                |e| matches!(e, E::ProbeArgsNotObject),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": {} },
                    "until": { "matches": "[unclosed" },
                }),
                |e| matches!(e, E::InvalidPattern(_)),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": {} },
                    "until": { "quiet_for_sec": 0 },
                }),
                |e| matches!(e, E::ZeroQuietWindow),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": {} },
                    "until": { "matches": "x" },
                    "poll_sec": 0,
                }),
                |e| matches!(e, E::ZeroPollInterval),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": {} },
                    "until": { "matches": "x" },
                    "max_wait_sec": 0,
                }),
                |e| matches!(e, E::ZeroWaitBound),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": {} },
                    "until": { "matches": "x" },
                    "poll_sec": 120,
                    "max_wait_sec": 120,
                }),
                |e| matches!(e, E::PollExceedsBound { .. }),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": {} },
                    "until": { "matches": "x" },
                    "poll_sec": 120,
                    "max_wait_sec": 100,
                }),
                |e| matches!(e, E::PollExceedsBound { .. }),
            ),
            (
                serde_json::json!({
                    "probe": { "tool": "t", "args": {} },
                    "until": { "quiet_for_sec": 200 },
                    "max_wait_sec": 100,
                }),
                |e| matches!(e, E::QuietWindowExceedsBound { .. }),
            ),
        ];

        for (json, expected) in cases {
            let error = WaitForCall::parse(args(json.clone())).unwrap_err();
            assert!(expected(&error), "case {json}: got {error:?}");
        }
    }

    #[test]
    fn poll_above_clamped_bound_reports_effective_bound_and_ceiling() {
        let error = WaitForCall::parse(args(serde_json::json!({
            "probe": { "tool": "t", "args": {} },
            "until": { "matches": "x" },
            "poll_sec": 310,
            "max_wait_sec": 400,
        })))
        .unwrap_err();
        let WaitForCallError::PollExceedsBound {
            poll_secs,
            bound_secs,
            ceiling,
        } = error
        else {
            panic!("expected PollExceedsBound, got {error:?}");
        };
        assert_eq!(poll_secs, 310);
        assert_eq!(bound_secs, MAX_WAIT_HARD_CEILING_SECS);
        assert_eq!(ceiling, MAX_WAIT_HARD_CEILING_SECS);
        assert!(error.to_string().contains("300s"));
    }

    #[test]
    fn quiet_window_above_clamped_bound_reports_effective_bound_and_ceiling() {
        let error = WaitForCall::parse(args(serde_json::json!({
            "probe": { "tool": "t", "args": {} },
            "until": { "quiet_for_sec": 350 },
            "max_wait_sec": 400,
        })))
        .unwrap_err();
        let WaitForCallError::QuietWindowExceedsBound {
            quiet_secs,
            bound_secs,
            ceiling,
        } = error
        else {
            panic!("expected QuietWindowExceedsBound, got {error:?}");
        };
        assert_eq!(quiet_secs, 350);
        assert_eq!(bound_secs, MAX_WAIT_HARD_CEILING_SECS);
        assert_eq!(ceiling, MAX_WAIT_HARD_CEILING_SECS);
        assert!(error.to_string().contains("300s"));
    }

    #[test]
    fn multi_key_until_fails_wire_deserialization() {
        let result = serde_json::from_value::<WaitForArgs>(serde_json::json!({
            "probe": { "tool": "t", "args": {} },
            "until": { "matches": "x", "quiet_for_sec": 5 },
        }));
        assert!(result.is_err());
    }

    #[test]
    fn unknown_top_level_field_fails_wire_deserialization() {
        let result = serde_json::from_value::<WaitForArgs>(serde_json::json!({
            "probe": { "tool": "t", "args": {} },
            "until": { "matches": "x" },
            "poll_secs": 3,
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_wait_for_definition() {
        let def = WaitForTool::tool_definition();
        assert_eq!(def.name, "wait_for");
        assert!(def.description.contains("stop condition"));

        let props = def.parameters.get("properties").unwrap();
        assert!(props.get("probe").is_some());
        assert!(props.get("until").is_some());
        assert_eq!(props["poll_sec"]["default"], POLL_DEFAULT_SECS);
        assert_eq!(props["max_wait_sec"]["default"], MAX_WAIT_DEFAULT_SECS);
        assert_eq!(
            def.parameters["required"],
            serde_json::json!(["probe", "until"])
        );

        assert_eq!(props["until"]["oneOf"].as_array().unwrap().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_probe_cannot_outrun_the_bound() {
        let (tool, _) = tool(HangsAfterSample::new(0));
        let error = tool
            .call(args(serde_json::json!({
                "probe": { "tool": "hangs", "args": {} },
                "until": { "matches": "done" },
                "poll_sec": 1,
                "max_wait_sec": 5,
            })))
            .await
            .unwrap_err();
        let WaitForError::ProbeTimedOut { tool, bound_secs } = error else {
            panic!("expected ProbeTimedOut, got {error:?}");
        };
        assert_eq!(tool, "hangs");
        assert_eq!(bound_secs, 5);
    }

    #[tokio::test(start_paused = true)]
    async fn probe_that_hangs_after_a_sample_times_out_with_that_observation() {
        let (tool, _) = tool(HangsAfterSample::new(1));
        let output = tool
            .call(args(serde_json::json!({
                "probe": { "tool": "stalls", "args": {} },
                "until": { "matches": "never" },
                "poll_sec": 1,
                "max_wait_sec": 5,
            })))
            .await
            .unwrap();
        assert_eq!(output.reason, StopReason::Timeout);
        assert_eq!(output.last_observation, "working");
        assert_eq!(output.samples.get(), 1);
        assert_eq!(output.elapsed_sec, 5);
    }

    #[tokio::test(start_paused = true)]
    async fn observation_over_the_cap_is_rejected() {
        let oversized = "x".repeat(MAX_OBSERVATION_BYTES + 1);
        let (tool, _) = tool(ScriptedProbe::new(&[&oversized]));
        let error = tool
            .call(args(serde_json::json!({
                "probe": { "tool": "firehose", "args": {} },
                "until": { "matches": "done" },
            })))
            .await
            .unwrap_err();
        let WaitForError::ObservationTooLarge {
            sample,
            bytes,
            limit,
            ..
        } = error
        else {
            panic!("expected ObservationTooLarge, got {error:?}");
        };
        assert_eq!(sample.get(), 1);
        assert_eq!(bytes, MAX_OBSERVATION_BYTES + 1);
        assert_eq!(limit, MAX_OBSERVATION_BYTES);
    }
}
