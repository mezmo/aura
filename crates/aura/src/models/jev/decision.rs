//! Lowering config and tool questions into a jev-driver decision.

use aura_config::JevQuestion;
use jev_driver::JevError;
use jev_driver::question::{BuiltQuestion, Decision as DriverDecision};
use jev_driver::wire::{Instructions, NoulCriteria, WireQuestion};
use serde_json::Value;

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

#[cfg(test)]
mod tests {
    use super::*;

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
