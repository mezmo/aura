//! What the runtime records on a run's `agent.stream` span when the run
//! starts and when it ends.

use aura_events::run::{RunCancelReason, RunEnding};
use aura_events::{RunId, SessionId};

use crate::conversation::LaneNumber;
use crate::logging;

/// How a run is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunMode {
    /// One prepared agent, with its MCP tool schemas as JSON documents.
    SingleAgent {
        tools_json: Vec<String>,
    },
    Orchestration,
}

/// The values a run's `agent.stream` span records about the run, including
/// the mode that goes in the `aura.mode` attribute.
pub(crate) struct RunSpan {
    pub provider: String,
    pub model: String,
    pub run: RunId,
    pub session: SessionId,
    /// The conversation's lane the run is on.
    pub lane: Option<LaneNumber>,
    pub prompt: String,
    pub user: Option<String>,
    /// The request's `metadata`, as one JSON object.
    pub metadata_json: Option<String>,
    /// The agent's LLM settings, as one JSON object.
    pub invocation_parameters: Option<String>,
    /// The number of messages in the history the run started with.
    pub history_len: usize,
    pub system_prompt: Option<String>,
    pub mode: RunMode,
}

impl RunSpan {
    /// Records the input side on `span`: the provider and model, the prompt,
    /// the system prompt, the run and session ids, the lane's number as
    /// `session.lane` for a run on a lane, the `aura.version` attribute, the
    /// mode, the user and metadata if set, the invocation parameters, a
    /// single agent's tool schemas, and the message count (the history plus
    /// the prompt). The version comes from the crate's version constant. The
    /// attributes go on in the same order as on the request-bound path, so
    /// the two paths' spans compare equal.
    pub(crate) fn record_input(&self, span: &tracing::Span) {
        logging::set_llm_identifiers(span, &self.provider, &self.model);
        logging::set_input_attributes(span, &self.prompt);
        if let Some(system_prompt) = &self.system_prompt {
            logging::set_system_prompt_attribute(span, system_prompt);
        }
        logging::set_span_attribute(span, "http.request_id", self.run.to_string());
        logging::set_span_attribute(span, "session.id", self.session.to_string());
        if let Some(lane) = self.lane {
            logging::set_span_attribute(span, "session.lane", i64::from(lane.get()));
        }
        logging::set_span_attribute(span, logging::ATTR_AURA_VERSION, logging::AURA_VERSION);
        let (mode, tools_json): (&'static str, &[String]) = match &self.mode {
            RunMode::SingleAgent { tools_json } => ("single-agent", tools_json),
            RunMode::Orchestration => ("orchestration", &[]),
        };
        logging::set_span_attribute(span, logging::ATTR_AURA_MODE, mode);
        if let Some(user) = &self.user {
            logging::set_span_attribute(span, logging::ATTR_USER_ID, user.clone());
        }
        if let Some(metadata) = &self.metadata_json {
            logging::set_span_attribute(span, logging::ATTR_METADATA, metadata.clone());
        }
        if let Some(parameters) = &self.invocation_parameters {
            logging::set_llm_invocation_parameters(span, parameters);
        }
        if !tools_json.is_empty() {
            logging::set_llm_tools(span, tools_json);
        }
        logging::set_span_attribute(span, "message_count", (self.history_len + 1) as i64);
    }

    /// Records the output side on `span`: `content`, the run's answer,
    /// unless the answer is empty (the request-bound path skips an empty
    /// answer too), then the ending through [`record_ending`].
    pub(crate) fn record_output(
        &self,
        span: &tracing::Span,
        end: &RunEnding,
        content: Option<&str>,
    ) {
        if let Some(content) = content.filter(|content| !content.is_empty()) {
            logging::set_output_attributes(span, content);
        }
        record_ending(span, end);
    }
}

/// Records on a run's `agent.stream` span how the run ended: the
/// `stream.termination` attribute and the status that `termination` gives
/// for `end`.
pub fn record_ending(span: &tracing::Span, end: &RunEnding) {
    let Termination { attribute, status } = termination(end);
    if let Some(attribute) = attribute {
        logging::set_span_attribute(span, "stream.termination", attribute);
    }
    match status {
        SpanStatus::Ok => logging::set_span_ok(span),
        SpanStatus::Error(error) => logging::set_span_error(span, error),
        SpanStatus::Unset => {}
    }
}

/// The status a run's span ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpanStatus {
    Ok,
    /// An error, with its text.
    Error(String),
    /// No status: neither ok nor an error.
    Unset,
}

/// What a run's ending records on its span: the `stream.termination`
/// attribute and the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Termination {
    pub attribute: Option<&'static str>,
    pub status: SpanStatus,
}

/// What the span records for each way a run ends:
///
/// - Finished: an ok status and no `stream.termination` attribute.
/// - Failed: an error status with the failure's text, cut to the OTel
///   content limit the same way the request-bound path cuts it.
/// - Cancelled at its deadline: the `timeout` error status.
/// - Cancelled for a shutdown: `stream.termination` set to `shutdown`, and
///   no status.
/// - Cancelled because its last claimant left: `stream.termination` set to
///   `disconnected`, and no status.
/// - Cancelled externally, or for a reason this version does not know:
///   `stream.termination` set to `cancelled`, and no status.
/// - Parked: `stream.termination` set to `parked`, and no status.
///
/// The first five match what the request-bound path records for its own
/// terminations, so the two paths' spans agree. That path has nothing like
/// the last two.
pub(crate) fn termination(end: &RunEnding) -> Termination {
    let (attribute, status) = match end {
        RunEnding::Finished => (None, SpanStatus::Ok),
        RunEnding::Failed { error } => (None, SpanStatus::Error(logging::truncate_for_otel(error))),
        RunEnding::Cancelled { reason, .. } => match reason {
            RunCancelReason::Deadline { .. } => (None, SpanStatus::Error("timeout".to_owned())),
            RunCancelReason::Shutdown => (Some("shutdown"), SpanStatus::Unset),
            RunCancelReason::Unclaimed => (Some("disconnected"), SpanStatus::Unset),
            RunCancelReason::External => (Some("cancelled"), SpanStatus::Unset),
            _ => (Some("cancelled"), SpanStatus::Unset),
        },
        RunEnding::Parked => (Some("parked"), SpanStatus::Unset),
    };
    Termination { attribute, status }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cancelled(reason: RunCancelReason) -> RunEnding {
        RunEnding::Cancelled {
            reason,
            message: None,
        }
    }

    #[test]
    fn every_ending_maps_and_only_a_failure_or_a_deadline_is_an_error() {
        let cases = [
            (RunEnding::Finished, None, SpanStatus::Ok),
            (
                RunEnding::Failed {
                    error: "boom".to_owned(),
                },
                None,
                SpanStatus::Error("boom".to_owned()),
            ),
            (
                cancelled(RunCancelReason::Deadline {
                    after: Duration::from_secs(1),
                }),
                None,
                SpanStatus::Error("timeout".to_owned()),
            ),
            (
                cancelled(RunCancelReason::Shutdown),
                Some("shutdown"),
                SpanStatus::Unset,
            ),
            (
                cancelled(RunCancelReason::Unclaimed),
                Some("disconnected"),
                SpanStatus::Unset,
            ),
            (
                cancelled(RunCancelReason::External),
                Some("cancelled"),
                SpanStatus::Unset,
            ),
            (RunEnding::Parked, Some("parked"), SpanStatus::Unset),
        ];
        for (end, attribute, status) in cases {
            assert_eq!(
                termination(&end),
                Termination { attribute, status },
                "{end:?}"
            );
        }
    }

    #[test]
    fn a_long_failure_is_cut_as_the_request_bound_path_cuts_it() {
        let long = "x".repeat(100_000);
        let Termination { status, .. } = termination(&RunEnding::Failed {
            error: long.clone(),
        });
        let SpanStatus::Error(cut) = &status else {
            panic!("a failure is an error: {status:?}");
        };
        assert!(
            cut.len() < long.len(),
            "the text is cut: {} bytes",
            cut.len()
        );
        let kept = cut
            .strip_suffix("...")
            .unwrap_or_else(|| panic!("the cut ends with the marker: {cut:?}"));
        assert!(
            long.starts_with(kept),
            "the cut keeps the start of the text"
        );
        assert_eq!(status, SpanStatus::Error(logging::truncate_for_otel(&long)));
    }

    #[test]
    fn an_unknown_cancel_reason_reads_as_cancelled() {
        let unknown: RunCancelReason =
            serde_json::from_value(serde_json::json!({ "reason": "later" })).unwrap();
        assert_eq!(
            termination(&cancelled(unknown)),
            Termination {
                attribute: Some("cancelled"),
                status: SpanStatus::Unset,
            }
        );
    }
}
