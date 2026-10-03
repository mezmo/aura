//! Typed decode of the resume endpoint's answers.
//!
//! `POST /v1/sessions/{session_id}/runs/{run_id}` answers either a 200
//! carrying the resumed run's SSE stream, or a typed refusal: the 409
//! conflict rows `{code, detail, blocking}`, a bare 404, a 503
//! `reify_unavailable`, or a 500 `reify_failed`. This module decodes
//! those rows into [`ResumeOutcome`] so every consumer branches on the
//! variant, never on strings.

use crate::api::stream::StreamOutcome;

/// One outstanding parked call from a 409 `parked` row's `blocking` set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct BlockingCall {
    pub decision_id: String,
    pub tool: String,
    pub expires_at: String,
}

/// The outcome of one resume POST.
#[derive(Debug)]
pub enum ResumeOutcome {
    /// A 200: the resumed run's stream, already parsed, with its
    /// termination classified.
    Streamed(Box<StreamOutcome>),
    /// A retryable 409: the run sits at a gate; `blocking` carries the
    /// outstanding calls (empty on `running`).
    Parked {
        blocking: Vec<BlockingCall>,
    },
    /// A retryable 409: another resume holds the run.
    Running,
    /// Terminal 409 rows: no retry can change the answer.
    Interrupted,
    ConfigChanged,
    Mismatch,
    Expired,
    /// The run is absent (unknown, cleaned up after retention, or hidden
    /// for identity). Terminal.
    NotFound,
    /// A 503 `reify_unavailable`: known pre-execution I/O availability
    /// failure. Transient — counts against the reattach budget.
    Unavailable,
    /// A 500 `reify_failed`: corrupt or internal. Terminal.
    ReifyFailed,
}

impl ResumeOutcome {
    /// Decode a pre-stream refusal response into the typed outcome.
    ///
    /// `status` and `body` carry the refusal; a 200 never reaches this
    /// function (it streams instead). Unknown 409 codes and undecodable
    /// bodies fail closed to the terminal failed shape — never retryable,
    /// never transient.
    pub fn from_refusal(status: u16, body: &str) -> Self {
        match status {
            404 => Self::NotFound,
            503 => {
                // Only the typed availability row is transient: an
                // undecodable or differently-typed 503 fails closed —
                // treating an unknown fault as retryable would spend the
                // transient budget on a permanent condition.
                #[derive(serde::Deserialize)]
                struct ErrorBody {
                    error: ErrorDetail,
                }
                #[derive(serde::Deserialize)]
                struct ErrorDetail {
                    error_type: String,
                }
                match serde_json::from_str::<ErrorBody>(body) {
                    Ok(row) if row.error.error_type == "reify_unavailable" => Self::Unavailable,
                    _ => Self::ReifyFailed,
                }
            }
            409 => {
                #[derive(serde::Deserialize)]
                struct Row {
                    code: String,
                    #[serde(default)]
                    blocking: Vec<BlockingCall>,
                }
                match serde_json::from_str::<Row>(body) {
                    Ok(row) => match row.code.as_str() {
                        "parked" => Self::Parked {
                            blocking: row.blocking,
                        },
                        "running" => Self::Running,
                        "interrupted" => Self::Interrupted,
                        "config_changed" => Self::ConfigChanged,
                        "mismatch" => Self::Mismatch,
                        "expired" => Self::Expired,
                        _ => Self::ReifyFailed,
                    },
                    Err(_) => Self::ReifyFailed,
                }
            }
            // 500 `reify_failed` and anything else the client does not
            // know: terminal.
            _ => Self::ReifyFailed,
        }
    }

    /// Retryable 409 rows: polling again after a delay can still reach a
    /// 200.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Parked { .. } | Self::Running)
    }

    /// Transient availability failures: retried on the transient budget,
    /// distinct from retryable gate rows.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parked_row() -> String {
        serde_json::json!({
            "code": "parked",
            "detail": "the run waits on outstanding approvals",
            "blocking": [
                {
                    "decision_id": "0191-decision",
                    "tool": "deploy_production",
                    "expires_at": "2026-10-01T12:00:00Z"
                },
                {
                    "decision_id": "0191-second",
                    "tool": "drop_table",
                    "expires_at": "2026-10-01T12:05:00Z"
                }
            ]
        })
        .to_string()
    }

    fn conflict(code: &str) -> String {
        serde_json::json!({
            "code": code,
            "detail": "diagnostic",
            "blocking": []
        })
        .to_string()
    }

    /// One expected terminal variant, for the decode matrix.
    #[derive(Debug, Clone, Copy)]
    enum TerminalProbe {
        Interrupted,
        ConfigChanged,
        Mismatch,
        Expired,
    }

    impl TerminalProbe {
        fn matches(self, outcome: &ResumeOutcome) -> bool {
            matches!(
                (self, outcome),
                (Self::Interrupted, ResumeOutcome::Interrupted)
                    | (Self::ConfigChanged, ResumeOutcome::ConfigChanged)
                    | (Self::Mismatch, ResumeOutcome::Mismatch)
                    | (Self::Expired, ResumeOutcome::Expired)
            )
        }
    }

    #[test]
    fn parked_row_decodes_its_blocking_calls() {
        let outcome = ResumeOutcome::from_refusal(409, &parked_row());
        let ResumeOutcome::Parked { blocking } = outcome else {
            panic!("expected Parked, got {outcome:?}");
        };
        assert_eq!(blocking.len(), 2);
        assert_eq!(blocking[0].decision_id, "0191-decision");
        assert_eq!(blocking[0].tool, "deploy_production");
        assert_eq!(blocking[1].expires_at, "2026-10-01T12:05:00Z");
    }

    #[test]
    fn running_row_decodes_without_blocking() {
        let outcome = ResumeOutcome::from_refusal(409, &conflict("running"));
        assert!(matches!(outcome, ResumeOutcome::Running));
    }

    #[test]
    fn terminal_conflict_rows_decode_to_their_variants() {
        let cases: &[(&str, TerminalProbe)] = &[
            ("interrupted", TerminalProbe::Interrupted),
            ("config_changed", TerminalProbe::ConfigChanged),
            ("mismatch", TerminalProbe::Mismatch),
            ("expired", TerminalProbe::Expired),
        ];
        for (code, probe) in cases {
            let outcome = ResumeOutcome::from_refusal(409, &conflict(code));
            assert!(probe.matches(&outcome), "{code} decoded to {outcome:?}");
            assert!(!outcome.is_retryable());
        }
    }

    #[test]
    fn not_found_decodes_from_bare_404() {
        let outcome = ResumeOutcome::from_refusal(404, "");
        assert!(matches!(outcome, ResumeOutcome::NotFound));
    }

    #[test]
    fn unavailable_decodes_only_from_the_typed_503_row() {
        let outcome = ResumeOutcome::from_refusal(
            503,
            "{\"error\":{\"message\":\"approval storage is temporarily unavailable; retry the resume\",\"error_type\":\"reify_unavailable\"}}",
        );
        assert!(matches!(outcome, ResumeOutcome::Unavailable));
    }

    #[test]
    fn an_undecodable_or_differently_typed_503_fails_closed() {
        let untyped = ResumeOutcome::from_refusal(503, "gateway hiccup");
        assert!(matches!(untyped, ResumeOutcome::ReifyFailed));
        let mistyped = ResumeOutcome::from_refusal(
            503,
            "{\"error\":{\"message\":\"else\",\"error_type\":\"something_else\"}}",
        );
        assert!(matches!(mistyped, ResumeOutcome::ReifyFailed));
    }

    #[test]
    fn reify_failed_decodes_from_500() {
        let outcome = ResumeOutcome::from_refusal(
            500,
            "{\"error\":{\"message\":\"the paused run could not be restored\",\"error_type\":\"reify_failed\"}}",
        );
        assert!(matches!(outcome, ResumeOutcome::ReifyFailed));
    }

    #[test]
    fn unknown_conflict_code_fails_closed_as_terminal() {
        // A future 409 code the client does not know must never read as
        // retryable: fail closed to the terminal failed shape.
        let outcome = ResumeOutcome::from_refusal(409, &conflict("novel_future_code"));
        assert!(!outcome.is_retryable());
        assert!(!outcome.is_transient());
    }

    #[test]
    fn undecodable_409_body_fails_closed_as_terminal() {
        let outcome = ResumeOutcome::from_refusal(409, "not json at all");
        assert!(!outcome.is_retryable());
        assert!(!outcome.is_transient());
    }

    #[test]
    fn retryable_and_transient_classifications() {
        assert!(ResumeOutcome::from_refusal(409, &parked_row()).is_retryable());
        assert!(ResumeOutcome::from_refusal(409, &conflict("running")).is_retryable());
        assert!(
            ResumeOutcome::from_refusal(503, r#"{"error":{"error_type":"reify_unavailable"}}"#)
                .is_transient()
        );
        assert!(!ResumeOutcome::from_refusal(404, "").is_retryable());
        assert!(!ResumeOutcome::from_refusal(409, &conflict("expired")).is_retryable());
    }
}
