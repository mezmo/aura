//! A serializable view of jev-driver's validated answers.
//!
//! jev-driver checks every answer against the question that asked it
//! (`jev_driver::Answers::from_wire`); this view lays the validated answers
//! out in ask order for task results, tool output and summaries.

use std::collections::BTreeMap;

use jev_driver::JevError;
use jev_driver::answer::{Answer as DriverAnswer, Answers as DriverAnswers};
use serde::Serialize;

use super::decision::Decision;

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

#[cfg(test)]
mod tests {
    use aura_config::JevQuestion;
    use serde_json::json;

    use super::*;

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
