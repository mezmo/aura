//! Jev (TypeSafe System One) decision workers.
//!
//! The client, wire format and answer validation come from the
//! [`jev_driver`] crate. This module holds aura's side: lowering config and
//! tool questions into a jev-driver decision, the serializable view of its
//! answers, and the worker wrapper that pairs a configured worker with its
//! client. The coordinator tools live in
//! [`crate::orchestration::tools::decision`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use aura_config::{JevQuestion, JevWorkerConfig, OrchestrationConfig};
use jev_driver::JevError;
use jev_driver::answer::{Answer as DriverAnswer, Answers as DriverAnswers};
use jev_driver::client::{DEFAULT_MODEL, JevClient, JevConfig};
use jev_driver::question::{BuiltQuestion, Decision as DriverDecision};
use jev_driver::wire::{Instructions, NoulCriteria, WireQuestion};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

// ============================================================================
// Decision: config and tool questions → jev-driver
// ============================================================================

/// A jev-driver decision plus the questions in the order they were asked.
#[derive(Debug, Clone)]
pub struct Decision {
    inner: DriverDecision,
    /// `(id, instruction)` in ask order.
    asked: Vec<(String, String)>,
}

impl Decision {
    /// Build a decision from questions in ask order.
    ///
    /// A question without an `id` is named `q<position>`, its index in
    /// `questions`, so a caller that concatenates question lists gets ids
    /// that cannot collide with each other. Every question is validated, and
    /// jev-driver rejects an id used twice.
    pub fn new(state: Value, questions: &[JevQuestion]) -> Result<Self, JevError> {
        if questions.is_empty() {
            return Err(JevError::Schema("no questions to ask".to_string()));
        }
        let mut builder = DriverDecision::builder().state(state)?;
        let mut asked = Vec::with_capacity(questions.len());
        for (index, question) in questions.iter().enumerate() {
            question.validate().map_err(JevError::Schema)?;
            let id = question
                .id()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("q{index}"));
            builder = builder.with_question(BuiltQuestion {
                id: id.clone(),
                question: lower(question),
            })?;
            asked.push((id, question.instruction().to_string()));
        }
        Ok(Self {
            inner: builder.build()?,
            asked,
        })
    }

    /// The decision to send.
    pub fn driver(&self) -> &DriverDecision {
        &self.inner
    }

    /// Question ids in ask order.
    pub fn order(&self) -> Vec<String> {
        self.asked.iter().map(|(id, _)| id.clone()).collect()
    }

    /// `(id, instruction)` in ask order.
    pub fn asked(&self) -> &[(String, String)] {
        &self.asked
    }
}

/// Lower a config/tool question to its wire form. An empty choice
/// description sends the option key alone.
fn lower(question: &JevQuestion) -> WireQuestion {
    match question {
        JevQuestion::Choice {
            instruction,
            criteria,
            ..
        } => WireQuestion::Choice {
            instructions: Instructions::text(instruction),
            criteria: criteria
                .iter()
                .map(|(key, description)| {
                    let description =
                        (!description.trim().is_empty()).then(|| Instructions::text(description));
                    (key.clone(), description)
                })
                .collect(),
        },
        JevQuestion::Score {
            instruction,
            criteria,
            ..
        } => WireQuestion::Score {
            instructions: Instructions::text(instruction),
            criteria: criteria.iter().map(Instructions::text).collect(),
        },
        JevQuestion::Noul {
            instruction,
            yes,
            no,
            ..
        } => WireQuestion::Noul {
            instructions: Instructions::text(instruction),
            criteria: yes.clone().zip(no.clone()).map(|(yes, no)| NoulCriteria {
                yes: Instructions::text(yes),
                no: Instructions::text(no),
            }),
        },
    }
}

// ============================================================================
// Answers: the serializable view of validated answers
// ============================================================================

/// One answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Choice {
        selected: String,
        /// Option key → probability.
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        /// Probability-weighted level; may fall between levels.
        score: f64,
        /// Description of the most probable level.
        level: String,
        /// Level number → probability.
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Noul {
        /// P(yes).
        probability: f64,
    },
}

impl Answer {
    /// Certainty in [0, 1]: the model's confidence for a choice or score;
    /// for a noul, how far the probability sits from a coin flip.
    pub fn certainty(&self) -> f64 {
        match self {
            Self::Choice { confidence, .. } | Self::Score { confidence, .. } => *confidence,
            Self::Noul { probability } => (2.0 * probability - 1.0).abs(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Choice {
                selected,
                probabilities,
                confidence,
            } => format!(
                "{selected} (p={:.2}, confidence={confidence:.2})",
                probabilities.get(selected).copied().unwrap_or_default()
            ),
            Self::Score {
                score,
                level,
                confidence,
                ..
            } => format!("{score:.2} — {level} (confidence={confidence:.2})"),
            Self::Noul { probability } => format!("P(yes)={probability:.2}"),
        }
    }

    fn from_driver(answer: &DriverAnswer) -> Self {
        let probabilities = |map: &BTreeMap<String, jev_driver::answer::Probability>| {
            map.iter()
                .map(|(key, p)| (key.clone(), p.get()))
                .collect::<BTreeMap<_, _>>()
        };
        match answer {
            DriverAnswer::Choice(data) => Self::Choice {
                selected: data.selected().to_string(),
                probabilities: probabilities(data.probabilities()),
                confidence: data.confidence().get(),
            },
            DriverAnswer::Score(data) => {
                let probabilities = probabilities(data.probabilities());
                let level = probabilities
                    .iter()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .and_then(|(key, _)| data.legend().get(key))
                    .cloned()
                    .unwrap_or_default();
                Self::Score {
                    score: data.score(),
                    level,
                    probabilities,
                    confidence: data.confidence().get(),
                }
            }
            DriverAnswer::Noul(probability) => Self::Noul {
                probability: probability.get(),
            },
        }
    }
}

/// One question and its answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnsweredQuestion {
    pub id: String,
    pub instruction: String,
    pub answer: Answer,
}

/// Token accounting for one evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Every answer of one evaluation, in ask order.
#[derive(Debug, Clone, Serialize)]
pub struct Answers {
    /// The model that answered.
    pub model: String,
    pub answers: Vec<AnsweredQuestion>,
    pub usage: Usage,
}

impl Answers {
    /// Lay out validated answers in the decision's ask order.
    pub fn from_driver(answers: &DriverAnswers, decision: &Decision) -> Result<Self, JevError> {
        let answers_in_order = decision
            .asked()
            .iter()
            .map(|(id, instruction)| {
                Ok(AnsweredQuestion {
                    id: id.clone(),
                    instruction: instruction.clone(),
                    answer: Answer::from_driver(answers.get(id)?),
                })
            })
            .collect::<Result<_, JevError>>()?;
        Ok(Self {
            model: answers.model.clone(),
            answers: answers_in_order,
            usage: Usage {
                input_tokens: answers.usage.input_tokens,
                output_tokens: answers.usage.output_tokens,
            },
        })
    }

    /// One line per answer: `<id>: <instruction> → <answer>`.
    pub fn summary(&self) -> String {
        self.answers
            .iter()
            .map(|a| format!("{}: {} → {}", a.id, a.instruction, a.answer.describe()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The least certain answer's certainty.
    pub fn min_certainty(&self) -> Option<f64> {
        self.answers
            .iter()
            .map(|a| a.answer.certainty())
            .min_by(f64::total_cmp)
    }
}

// ============================================================================
// State limits
// ============================================================================

/// Most characters of state sent to Jev in one evaluation.
pub(crate) const JEV_MAX_STATE_CHARS: usize = 32_000;
/// Most characters of coordinator-staged state in one task's state.
pub(crate) const JEV_MAX_STAGED_STATE_CHARS: usize = 8_000;
/// Most characters of task description in one task's state.
pub(crate) const JEV_MAX_TASK_CHARS: usize = 4_000;
/// Fewest characters kept of each upstream result or attached artifact.
pub(crate) const JEV_MIN_PART_CHARS: usize = 1_000;

/// `text` cut to at most `max` bytes on a char boundary, marked when cut.
pub(crate) fn truncate_marked(text: &str, max: usize) -> String {
    let (kept, truncated) = crate::string_utils::safe_truncate(text, max);
    if truncated {
        format!("{kept}… [truncated]")
    } else {
        kept.to_string()
    }
}

/// Reject a state whose serialized form is longer than `max` characters,
/// with an error a model can act on.
pub(crate) fn check_state_size(state: &Value, max: usize) -> Result<(), String> {
    let len = state.to_string().len();
    if len > max {
        return Err(format!(
            "`state` is {len} characters; keep it under {max}. Put bulk data in an \
             artifact (write_artifact) and attach it to the task instead."
        ));
    }
    Ok(())
}

// ============================================================================
// Worker: a configured Jev worker and its client
// ============================================================================

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One `type = "jev"` worker, ready to evaluate.
#[derive(Clone)]
pub struct JevWorker {
    pub name: String,
    pub config: JevWorkerConfig,
    pub client: JevClient,
    /// The Jev model id requests are sent with.
    pub model: String,
}

impl std::fmt::Debug for JevWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevWorker")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// Every Jev worker of an orchestration, by name.
pub type JevWorkers = Arc<BTreeMap<String, JevWorker>>;

impl JevWorker {
    /// Build the worker's client. A `base_url` without a path gets
    /// `v1/systemone` joined on and one with a path is the complete endpoint
    /// (jev-driver's `JevConfig::base`); no `base_url` is the TypeSafe cloud.
    /// An empty `api_key` sends no bearer token.
    pub fn new(name: impl Into<String>, config: JevWorkerConfig) -> Result<Self, JevError> {
        let mut client_config = match config.base_url.as_deref() {
            Some(raw) => JevConfig::base(
                url::Url::parse(raw)
                    .map_err(|e| JevError::Transport(format!("invalid base_url `{raw}`: {e}")))?,
            )?,
            None => JevConfig::cloud()?,
        };
        client_config.api_key = Some(config.api_key.trim().to_owned()).filter(|k| !k.is_empty());
        client_config.model = config
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        client_config.timeout = config
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TIMEOUT);
        let model = client_config.model.clone();
        let client = JevClient::new(client_config)?;
        Ok(Self {
            name: name.into(),
            config,
            client,
            model,
        })
    }

    /// Build a client for every Jev worker in `config`.
    pub fn all_from(config: &OrchestrationConfig) -> Result<JevWorkers, JevError> {
        let mut workers = BTreeMap::new();
        for (name, worker) in &config.workers {
            if let Some(jev) = worker.as_jev() {
                workers.insert(name.clone(), Self::new(name.clone(), jev.clone())?);
            }
        }
        Ok(Arc::new(workers))
    }

    /// The configured questions followed by `added`, in ask order.
    pub fn questions_with(&self, added: &[JevQuestion]) -> Vec<JevQuestion> {
        self.config.questions.iter().chain(added).cloned().collect()
    }

    /// Evaluate `questions` against `state`, sending the worker's preamble
    /// beside the state when one is configured.
    ///
    /// Cancelling `cancel` aborts the evaluation at once, an in-flight
    /// request included: jev-driver checks its token only between retries,
    /// so the call is raced against the token and dropped when it fires.
    pub async fn evaluate(
        &self,
        state: Value,
        questions: &[JevQuestion],
        cancel: &CancellationToken,
    ) -> Result<Answers, JevError> {
        let decision = Decision::new(self.with_preamble(state), questions)?;
        let answers = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(JevError::Cancelled),
            answers = self
                .client
                .evaluate_raw_with_cancellation(decision.driver(), cancel) => answers?,
        };
        Answers::from_driver(&answers, &decision)
    }

    fn with_preamble(&self, state: Value) -> Value {
        match self.config.preamble.as_deref().map(str::trim) {
            Some(preamble) if !preamble.is_empty() => {
                serde_json::json!({ "context": preamble, "state": state })
            }
            _ => state,
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! Test transports over jev-driver's: no network.

    use async_trait::async_trait;
    use jev_driver::JevError;
    use jev_driver::client::Transport;
    use jev_driver::wire::WireResponse;
    use std::sync::Arc;
    /// jev-driver's scripted [`jev_driver::fake::FakeTransport`], scripted with
    /// JSON response bodies (`Ok`) and HTTP statuses (`Err`).
    pub struct FakeTransport(jev_driver::fake::FakeTransport);

    impl FakeTransport {
        pub fn new(script: impl IntoIterator<Item = Result<serde_json::Value, u16>>) -> Arc<Self> {
            let script = script
                .into_iter()
                .map(|step| match step {
                    Ok(body) => serde_json::from_value::<WireResponse>(body)
                        .map_err(|e| JevError::Transport(format!("invalid response body: {e}"))),
                    Err(status) => Err(status_error(status)),
                })
                .collect();
            Arc::new(Self(jev_driver::fake::FakeTransport::new(script)))
        }

        /// Request bodies received so far.
        pub fn requests(&self) -> Vec<String> {
            self.0.requests()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn post_systemone(&self, body: String) -> Result<WireResponse, JevError> {
            self.0.post_systemone(body).await
        }
    }

    /// A transport whose requests never complete.
    pub struct HangingTransport;

    #[async_trait]
    impl Transport for HangingTransport {
        async fn post_systemone(&self, _body: String) -> Result<WireResponse, JevError> {
            std::future::pending().await
        }
    }

    /// The error jev-driver's HTTP transport reports for `status`.
    fn status_error(status: u16) -> JevError {
        match status {
            401 => JevError::Unauthorized,
            422 => JevError::InvalidRequest(format!("status {status}")),
            429 => JevError::RateLimited { retries: 0 },
            529 => JevError::Overloaded { retries: 0 },
            _ => JevError::Transport(format!("unexpected status {status}")),
        }
    }
}

#[cfg(test)]
mod tests {
    mod decision {
        use super::super::*;

        fn noul(id: Option<&str>) -> JevQuestion {
            JevQuestion::Noul {
                id: id.map(str::to_owned),
                instruction: "Is it real?".to_string(),
                yes: None,
                no: None,
            }
        }

        #[test]
        fn unnamed_questions_are_named_by_position() {
            let decision =
                Decision::new(Value::Null, &[noul(Some("real")), noul(None), noul(None)]).unwrap();
            assert_eq!(decision.order(), ["real", "q1", "q2"]);
        }

        #[test]
        fn duplicate_ids_are_rejected() {
            let err = Decision::new(Value::Null, &[noul(Some("q1")), noul(None)]).unwrap_err();
            assert!(
                err.to_string().contains("duplicate question id `q1`"),
                "{err}"
            );
        }

        #[test]
        fn empty_question_set_is_rejected() {
            assert!(matches!(
                Decision::new(Value::Null, &[]),
                Err(JevError::Schema(_))
            ));
        }

        #[test]
        fn invalid_questions_are_rejected_before_sending() {
            let one_level = JevQuestion::Score {
                id: None,
                instruction: "Rate".to_string(),
                criteria: vec!["only".to_string()],
            };
            let err = Decision::new(Value::Null, &[one_level]).unwrap_err();
            assert!(err.to_string().contains("2-10 levels"), "{err}");
        }

        #[test]
        fn questions_lower_to_the_recorded_wire_shape() {
            let questions = [
                JevQuestion::Choice {
                    id: Some("department".to_string()),
                    instruction: "Which team should handle `ticket`?".to_string(),
                    criteria: [
                        ("billing", "Payments, invoicing, and refunds"),
                        ("other", ""),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                },
                JevQuestion::Score {
                    id: Some("frustration".to_string()),
                    instruction: "How frustrated is the customer?".to_string(),
                    criteria: vec!["Calm".to_string(), "Angry".to_string()],
                },
                JevQuestion::Noul {
                    id: Some("is_urgent".to_string()),
                    instruction: "Does `ticket` convey time pressure?".to_string(),
                    yes: Some("Explicit deadline".to_string()),
                    no: Some("No time pressure".to_string()),
                },
            ];
            let decision = Decision::new(serde_json::json!({"ticket": "t"}), &questions).unwrap();
            assert_eq!(
                serde_json::to_value(decision.driver().questions()).unwrap(),
                serde_json::json!({
                    "department": {
                        "type": "choice",
                        "instructions": "Which team should handle `ticket`?",
                        "criteria": {"billing": "Payments, invoicing, and refunds", "other": null}
                    },
                    "frustration": {
                        "type": "score",
                        "instructions": "How frustrated is the customer?",
                        "criteria": ["Calm", "Angry"]
                    },
                    "is_urgent": {
                        "type": "noul",
                        "instructions": "Does `ticket` convey time pressure?",
                        "criteria": {"true": "Explicit deadline", "false": "No time pressure"}
                    }
                })
            );
            assert_eq!(decision.driver().state()["ticket"], "t");
        }
    }

    mod answer {
        use aura_config::JevQuestion;
        use serde_json::json;

        use super::super::*;

        /// The questions behind jev-driver's recorded triage fixture.
        fn triage_decision() -> Decision {
            let choice = |id: &str, options: &[&str]| JevQuestion::Choice {
                id: Some(id.to_string()),
                instruction: format!("{id}?"),
                criteria: options
                    .iter()
                    .map(|o| (o.to_string(), format!("{o} desc")))
                    .collect(),
            };
            Decision::new(
                json!({"ticket": "charged twice"}),
                &[
                    choice("department", &["billing", "other", "technical"]),
                    JevQuestion::Score {
                        id: Some("frustration".to_string()),
                        instruction: "frustration?".to_string(),
                        criteria: vec![
                            "Calm and neutral".to_string(),
                            "Concerned but civil".to_string(),
                            "Very angry".to_string(),
                        ],
                    },
                    JevQuestion::Noul {
                        id: Some("is_urgent".to_string()),
                        instruction: "urgent?".to_string(),
                        yes: None,
                        no: None,
                    },
                    choice("owner", &["dana", "kai"]),
                ],
            )
            .unwrap()
        }

        /// jev-driver's recorded `triage.response.json`.
        fn triage_response() -> serde_json::Value {
            json!({
                "model": "jev-1.13.0",
                "answers": {
                    "department": {
                        "type": "choice", "choice": "billing",
                        "probabilities": {"billing": 0.88, "other": 0.0, "technical": 0.12},
                        "confidence": 0.81
                    },
                    "frustration": {
                        "type": "score", "score": 1.05,
                        "legend": {"0": "Calm and neutral", "1": "Concerned but civil", "2": "Very angry"},
                        "probabilities": {"0": 0.0, "1": 0.95, "2": 0.05},
                        "confidence": 0.92
                    },
                    "is_urgent": {"type": "noul", "noul": 0.95},
                    "owner": {
                        "type": "choice", "choice": "dana",
                        "probabilities": {"dana": 0.7, "kai": 0.3},
                        "confidence": 0.7
                    }
                },
                "usage": {"input_tokens": 307, "output_tokens": 34}
            })
        }

        #[test]
        fn recorded_response_lays_out_in_ask_order() {
            let decision = triage_decision();
            let driver = DriverAnswers::from_wire(
                serde_json::from_value(triage_response()).unwrap(),
                decision.driver(),
            )
            .unwrap();
            let answers = Answers::from_driver(&driver, &decision).unwrap();

            let ids: Vec<_> = answers.answers.iter().map(|a| a.id.as_str()).collect();
            assert_eq!(ids, ["department", "frustration", "is_urgent", "owner"]);
            assert_eq!(answers.usage.input_tokens, 307);
            assert!(matches!(
                &answers.answers[1].answer,
                Answer::Score { level, .. } if level == "Concerned but civil"
            ));
            assert_eq!(answers.min_certainty(), Some(0.7));
            let summary = answers.summary();
            assert!(
                summary.contains("department: department? → billing (p=0.88"),
                "{summary}"
            );
            assert!(
                summary.contains("is_urgent: urgent? → P(yes)=0.95"),
                "{summary}"
            );
        }
    }

    mod worker {
        use serde_json::json;

        use super::super::*;

        fn worker(preamble: Option<&str>) -> JevWorker {
            let config: aura_config::WorkerConfig = toml::from_str(&format!(
                r#"
                type = "jev"
                description = "d"
                {}

                [[questions]]
                id = "real"
                instruction = "Is it real?"
            "#,
                preamble
                    .map(|p| format!("preamble = {p:?}"))
                    .unwrap_or_default()
            ))
            .unwrap();
            JevWorker::new("triage", config.as_jev().unwrap().clone()).unwrap()
        }

        #[test]
        fn preamble_is_sent_beside_the_state() {
            assert_eq!(
                worker(Some("Pipeline alerts.")).with_preamble(json!({"a": 1})),
                json!({"context": "Pipeline alerts.", "state": {"a": 1}})
            );
            assert_eq!(worker(None).with_preamble(json!({"a": 1})), json!({"a": 1}));
        }

        #[test]
        fn configured_questions_come_first() {
            let added = JevQuestion::Noul {
                id: None,
                instruction: "Extra?".to_string(),
                yes: None,
                no: None,
            };
            let questions = worker(None).questions_with(std::slice::from_ref(&added));
            assert_eq!(questions.len(), 2);
            assert_eq!(questions[0].id(), Some("real"));
            assert_eq!(questions[1], added);
        }

        fn noul() -> Vec<JevQuestion> {
            vec![JevQuestion::Noul {
                id: Some("real".to_string()),
                instruction: "Is it real?".to_string(),
                yes: None,
                no: None,
            }]
        }

        fn with_transport(
            transport: Arc<dyn jev_driver::client::Transport>,
            retry: jev_driver::client::RetryConfig,
        ) -> JevWorker {
            let mut worker = worker(None);
            worker.client = JevClient::with_transport(transport, "jev-test", retry);
            worker
        }

        #[tokio::test]
        async fn evaluate_sends_the_decision_and_lays_out_answers() {
            let fake = super::super::fake::FakeTransport::new([Ok(json!({
                "model": "jev-test",
                "answers": {"real": {"type": "noul", "noul": 0.9}},
                "usage": {"input_tokens": 10, "output_tokens": 1}
            }))]);
            let worker = with_transport(fake.clone(), Default::default());
            let answers = worker
                .evaluate(
                    json!({"alert": "disk full"}),
                    &noul(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(answers.answers[0].id, "real");
            let sent: Value = serde_json::from_str(&fake.requests()[0]).unwrap();
            assert_eq!(sent["model"], "jev-test");
            assert_eq!(sent["state"]["alert"], "disk full");
            assert_eq!(sent["questions"]["real"]["type"], "noul");
        }

        #[tokio::test]
        async fn cancellation_aborts_an_in_flight_request() {
            let worker = with_transport(
                Arc::new(super::super::fake::HangingTransport),
                Default::default(),
            );
            let cancel = CancellationToken::new();
            let canceller = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                canceller.cancel();
            });
            let err = worker
                .evaluate(json!({}), &noul(), &cancel)
                .await
                .unwrap_err();
            assert!(matches!(err, JevError::Cancelled), "{err}");
        }

        #[tokio::test]
        async fn cancellation_aborts_a_backoff_wait() {
            let fake = super::super::fake::FakeTransport::new([Err(429), Err(429)]);
            let slow = jev_driver::client::RetryConfig {
                max_retries: 3,
                initial_interval: Duration::from_secs(60),
                max_interval: Duration::from_secs(60),
                multiplier: 1.0,
            };
            let worker = with_transport(fake.clone(), slow);
            let cancel = CancellationToken::new();
            let canceller = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                canceller.cancel();
            });
            let err = worker
                .evaluate(json!({}), &noul(), &cancel)
                .await
                .unwrap_err();
            assert!(matches!(err, JevError::Cancelled), "{err}");
            assert_eq!(fake.requests().len(), 1);
        }

        #[test]
        fn new_defaults_the_model_and_hides_the_key() {
            let config: aura_config::WorkerConfig = toml::from_str(
                r#"
                type = "jev"
                description = "d"
                api_key = "secret-key"
                base_url = "http://localhost:8080"
            "#,
            )
            .unwrap();
            let worker = JevWorker::new("triage", config.as_jev().unwrap().clone()).unwrap();
            assert_eq!(worker.model, DEFAULT_MODEL);
            assert!(!format!("{worker:?}").contains("secret-key"));
        }

        #[test]
        fn new_rejects_an_unparseable_base_url() {
            let config: aura_config::WorkerConfig = toml::from_str(
                r#"
                type = "jev"
                description = "d"
                base_url = "not a url"
            "#,
            )
            .unwrap();
            let err = JevWorker::new("triage", config.as_jev().unwrap().clone()).unwrap_err();
            assert!(err.to_string().contains("invalid base_url"), "{err}");
        }
    }
}
