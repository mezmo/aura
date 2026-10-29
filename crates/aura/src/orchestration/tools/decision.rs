//! Coordinator tools backed by the configured Jev workers.
//!
//! `evaluate_decision` asks Jev right away and returns the answers;
//! `prepare_decision` stages questions and state for a planned Jev task,
//! which a `create_plan` step names in `decision_request`. Both speak of
//! "decisions" rather than Jev so the model reads what comes back —
//! probability distributions over fixed options, not text — and so another
//! decision model can stand behind the same tools.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use aura_config::JevQuestion;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::orchestration::jev::{
    Answers, Decision, JEV_MAX_STAGED_STATE_CHARS, JEV_MAX_STATE_CHARS, JevWorker, JevWorkers,
    check_state_size,
};
use crate::run_context::BoundRun;

/// Questions and state the coordinator prepared for one planned Jev task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub name: String,
    /// The Jev worker the request is for.
    pub worker: String,
    /// Coordinator-written questions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<JevQuestion>,
    /// Coordinator-supplied state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
}

/// A plan step's decision request: the name the coordinator gave it, or the
/// request itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecisionRequestRef {
    Named(String),
    Resolved(DecisionRequest),
}

impl DecisionRequestRef {
    pub fn resolved(&self) -> Option<&DecisionRequest> {
        match self {
            Self::Resolved(request) => Some(request),
            Self::Named(_) => None,
        }
    }
}

/// One run's prepared requests, by name.
pub type DecisionRequestStore = Arc<Mutex<HashMap<String, DecisionRequest>>>;

/// The decision tools for one coordinator.
#[derive(Clone)]
pub struct DecisionToolset {
    pub evaluate: EvaluateDecisionTool,
    pub prepare: PrepareDecisionTool,
}

impl DecisionToolset {
    /// The tool pair over `workers`, or `None` when there are no Jev workers
    /// to stand behind it. `run` is the coordinator's run slot, read at call
    /// time for the cancellation token; `usage` is the run's billed-usage
    /// totals, which every successful `evaluate_decision` adds its tokens to.
    pub fn new(
        workers: JevWorkers,
        store: DecisionRequestStore,
        run: Option<Arc<BoundRun>>,
        usage: crate::UsageState,
    ) -> Option<Self> {
        if workers.is_empty() {
            return None;
        }
        Some(Self {
            evaluate: EvaluateDecisionTool {
                workers: workers.clone(),
                run,
                usage,
            },
            prepare: PrepareDecisionTool { workers, store },
        })
    }

    /// Both tool definitions, for token accounting.
    pub async fn definitions(&self) -> [ToolDefinition; 2] {
        [
            self.evaluate.definition(String::new()).await,
            self.prepare.definition(String::new()).await,
        ]
    }
}

const QUESTIONS_HELP: &str = concat!(
    "`questions` must be a JSON array of question objects, for example: ",
    r#"[{"id": "owner", "instruction": "Which team owns this?", "options": {"web": "UI bugs", "infra": "Outages"}}, "#,
    r#"{"id": "severity", "instruction": "How severe is it?", "levels": ["minor", "major", "critical"]}, "#,
    r#"{"id": "real", "instruction": "The alert reflects a real problem"}]"#
);

fn question_schema() -> Value {
    json!({
        "type": "object",
        "required": ["instruction"],
        "properties": {
            "id": {
                "type": "string",
                "description": "Short name the answer is reported under (e.g. 'severity')."
            },
            "instruction": {
                "type": "string",
                "description": "The question. For a yes/no question, a statement to estimate the probability of."
            },
            "options": {
                "type": "object",
                "description": "Choice question: option key → what it means. Jev picks one.",
                "additionalProperties": { "type": "string" }
            },
            "levels": {
                "type": "array",
                "description": "Score question: 2-10 rubric levels, lowest first.",
                "items": { "type": "string" }
            },
            "yes": {
                "type": "string",
                "description": "Yes/no question only: what a yes means. Give with `no`."
            },
            "no": {
                "type": "string",
                "description": "Yes/no question only: what a no means. Give with `yes`."
            }
        }
    })
}

/// Parse the `questions` argument: an array, or a string holding one. Each
/// item takes `options` (choice) or `levels` (score), or `criteria` in the
/// config form; with none of them it is a yes/no question.
fn parse_questions(value: Option<Value>) -> Result<Vec<JevQuestion>, String> {
    let items = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(Value::String(raw)) => {
            crate::orchestration::tools::json_in_str::<Vec<Value>>(&raw, '[', ']')
                .ok_or_else(|| QUESTIONS_HELP.to_string())?
        }
        Some(_) => return Err(QUESTIONS_HELP.to_string()),
    };
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let Value::Object(mut map) = item else {
                return Err(format!(
                    "question {index} is not an object. {QUESTIONS_HELP}"
                ));
            };
            let options = map.remove("options");
            let levels = map.remove("levels");
            let criteria = match (options, levels) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "question {index} gives both `options` and `levels`; a question is \
                         either a choice or a score"
                    ));
                }
                (Some(c), None) | (None, Some(c)) => Some(c),
                (None, None) => None,
            };
            if let Some(criteria) = criteria
                && map.insert("criteria".to_string(), criteria).is_some()
            {
                return Err(format!(
                    "question {index} gives `criteria` alongside `options`/`levels`"
                ));
            }
            serde_json::from_value::<JevQuestion>(Value::Object(map))
                .map_err(|e| format!("question {index}: {e}"))
        })
        .collect()
}

/// Parse the `state` argument: an object or array as given, a string holding
/// JSON as that JSON, and any other string as text.
fn parse_state(value: Option<Value>) -> Option<Value> {
    match value? {
        Value::Null => None,
        Value::String(raw) => {
            let trimmed = raw.trim_start();
            let parsed = match trimmed.chars().next() {
                Some('{') => crate::orchestration::tools::json_in_str(&raw, '{', '}'),
                Some('[') => crate::orchestration::tools::json_in_str(&raw, '[', ']'),
                _ => None,
            };
            Some(parsed.unwrap_or(Value::String(raw)))
        }
        other => Some(other),
    }
}

/// The named worker, or the only one when no name is given.
fn resolve_worker<'a>(
    workers: &'a JevWorkers,
    name: Option<&str>,
) -> Result<&'a JevWorker, String> {
    let available = || workers.keys().cloned().collect::<Vec<_>>().join(", ");
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => workers.get(name).ok_or_else(|| {
            format!(
                "`{name}` is not a decision worker. Decision workers: {}",
                available()
            )
        }),
        None if workers.len() == 1 => Ok(workers.values().next().expect("one worker")),
        None => Err(format!(
            "name the decision worker to use. Decision workers: {}",
            available()
        )),
    }
}

fn worker_list(workers: &JevWorkers) -> String {
    workers
        .values()
        .map(|w| format!("'{}' ({})", w.name, w.config.description))
        .collect::<Vec<_>>()
        .join(", ")
}

// ============================================================================
// evaluate_decision
// ============================================================================

#[derive(Clone)]
pub struct EvaluateDecisionTool {
    workers: JevWorkers,
    run: Option<Arc<BoundRun>>,
    usage: crate::UsageState,
}

#[derive(Debug, Deserialize)]
pub struct EvaluateDecisionArgs {
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub questions: Option<Value>,
    #[serde(default)]
    pub state: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct EvaluateDecisionOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answers: Option<Answers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl EvaluateDecisionOutput {
    fn error(message: impl Into<String>) -> Self {
        Self {
            summary: None,
            answers: None,
            error: Some(format!("Error: {}", message.into())),
        }
    }
}

impl std::fmt::Display for EvaluateDecisionOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.error, &self.summary) {
            (Some(error), _) => write!(f, "{error}"),
            (None, Some(summary)) => write!(f, "{summary}"),
            (None, None) => Ok(()),
        }
    }
}

impl Tool for EvaluateDecisionTool {
    const NAME: &'static str = "evaluate_decision";

    type Error = Infallible;
    type Args = EvaluateDecisionArgs;
    type Output = EvaluateDecisionOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: format!(
                "Ask a decision model typed questions about a state you already have and get \
                 the answers now: for each question, a probability distribution over the \
                 options you define (choice), a score on your rubric (score), or the \
                 probability a statement is true (yes/no). It returns numbers, not prose, and \
                 cannot look anything up — put everything it should judge in `state`. Use it \
                 to triage, route, or check a result before planning; to judge data an \
                 upstream task will produce, plan a task for a decision worker and stage its \
                 questions with prepare_decision instead. Decision workers: {}.",
                worker_list(&self.workers)
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "worker": {
                        "type": "string",
                        "description": "Decision worker to ask. Optional when there is only one."
                    },
                    "questions": {
                        "type": "array",
                        "description": "The questions to answer.",
                        "items": question_schema()
                    },
                    "state": {
                        "type": "string",
                        "description": "What to judge: a JSON object, or plain text."
                    }
                },
                "required": ["questions", "state"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let worker = match resolve_worker(&self.workers, args.worker.as_deref()) {
            Ok(worker) => worker,
            Err(e) => return Ok(EvaluateDecisionOutput::error(e)),
        };
        let questions = match parse_questions(args.questions) {
            Ok(questions) if questions.is_empty() => {
                return Ok(EvaluateDecisionOutput::error(QUESTIONS_HELP));
            }
            Ok(questions) => questions,
            Err(e) => return Ok(EvaluateDecisionOutput::error(e)),
        };
        let Some(state) = parse_state(args.state) else {
            return Ok(EvaluateDecisionOutput::error(
                "`state` is required: the data the questions are about",
            ));
        };
        if let Err(e) = check_state_size(&state, JEV_MAX_STATE_CHARS) {
            return Ok(EvaluateDecisionOutput::error(e));
        }
        let cancel = self
            .run
            .as_ref()
            .and_then(|run| run.get())
            .map(|run| run.cancel_token().clone())
            .unwrap_or_default();
        match worker.evaluate(state, &questions, &cancel).await {
            Ok(answers) => {
                self.usage
                    .accumulate_usage(answers.usage.input_tokens, answers.usage.output_tokens);
                Ok(EvaluateDecisionOutput {
                    summary: Some(answers.summary()),
                    answers: Some(answers),
                    error: None,
                })
            }
            Err(e) => Ok(EvaluateDecisionOutput::error(e.to_string())),
        }
    }
}

// ============================================================================
// prepare_decision
// ============================================================================

#[derive(Clone)]
pub struct PrepareDecisionTool {
    workers: JevWorkers,
    store: DecisionRequestStore,
}

#[derive(Debug, Deserialize)]
pub struct PrepareDecisionArgs {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub questions: Option<Value>,
    #[serde(default)]
    pub state: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct PrepareDecisionOutput {
    pub status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub question_ids: Vec<String>,
}

impl std::fmt::Display for PrepareDecisionOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.status)
    }
}

impl PrepareDecisionOutput {
    fn error(message: impl Into<String>) -> Self {
        Self {
            status: format!("Error: {}", message.into()),
            question_ids: Vec::new(),
        }
    }
}

impl Tool for PrepareDecisionTool {
    const NAME: &'static str = "prepare_decision";

    type Error = Infallible;
    type Args = PrepareDecisionArgs;
    type Output = PrepareDecisionOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: format!(
                "Stage questions and extra state for a task you will assign to a decision \
                 worker, then name the request in that create_plan task's `decision_request`. \
                 The worker answers the staged questions after its configured ones, judging \
                 the task, the results of the tasks it depends on, and the state staged here. \
                 Preparing the same name again replaces the request. Decision workers: {}.",
                worker_list(&self.workers)
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name to reference from the task's `decision_request`."
                    },
                    "worker": {
                        "type": "string",
                        "description": "Decision worker the task is assigned to. Optional when there is only one."
                    },
                    "questions": {
                        "type": "array",
                        "description": "Questions to add to the worker's configured ones.",
                        "items": question_schema()
                    },
                    "state": {
                        "type": "string",
                        "description": "Extra context to judge beside the task: a JSON object, or plain text."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let name = args.name.trim().to_string();
        if name.is_empty() {
            return Ok(PrepareDecisionOutput::error("`name` is required"));
        }
        let worker = match resolve_worker(&self.workers, args.worker.as_deref()) {
            Ok(worker) => worker,
            Err(e) => return Ok(PrepareDecisionOutput::error(e)),
        };
        let questions = match parse_questions(args.questions) {
            Ok(questions) => questions,
            Err(e) => return Ok(PrepareDecisionOutput::error(e)),
        };
        let state = parse_state(args.state);
        if let Some(state) = &state
            && let Err(e) = check_state_size(state, JEV_MAX_STAGED_STATE_CHARS)
        {
            return Ok(PrepareDecisionOutput::error(e));
        }
        // Build the decision the task will send, so a bad or clashing
        // question is reported now rather than when the task runs.
        let decision = match Decision::new(Value::Null, &worker.questions_with(&questions)) {
            Ok(decision) => decision,
            Err(e) => {
                return Ok(PrepareDecisionOutput::error(format!(
                    "{e}. Worker '{}' asks {} configured question(s) first.",
                    worker.name,
                    worker.config.questions.len()
                )));
            }
        };
        let question_ids = decision.order().to_vec();
        let request = DecisionRequest {
            name: name.clone(),
            worker: worker.name.clone(),
            questions,
            state,
        };
        let replaced = self
            .store
            .lock()
            .expect("decision request store poisoned")
            .insert(name.clone(), request)
            .is_some();
        Ok(PrepareDecisionOutput {
            status: format!(
                "Decision request '{name}' {} for worker '{}' with {} question(s). Name it in \
                 the task's `decision_request`.",
                if replaced { "replaced" } else { "prepared" },
                worker.name,
                question_ids.len()
            ),
            question_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::orchestration::jev::fake::FakeTransport;
    use jev_driver::client::{JevClient, RetryConfig};

    fn workers(names: &[&str], configured: usize) -> JevWorkers {
        let mut map = BTreeMap::new();
        for name in names {
            let mut toml = format!("type = \"jev\"\ndescription = \"{name} desc\"\n");
            for i in 0..configured {
                toml.push_str(&format!(
                    "[[questions]]\nid = \"c{i}\"\ninstruction = \"Configured {i}?\"\n"
                ));
            }
            let config: aura_config::WorkerConfig = toml::from_str(&toml).unwrap();
            map.insert(
                name.to_string(),
                JevWorker::new(*name, config.as_jev().unwrap().clone()).unwrap(),
            );
        }
        Arc::new(map)
    }

    fn with_fake(workers: &JevWorkers, fake: Arc<FakeTransport>) -> JevWorkers {
        let mut map = (**workers).clone();
        for worker in map.values_mut() {
            worker.client =
                JevClient::with_transport(fake.clone(), "jev-test", RetryConfig::default());
        }
        Arc::new(map)
    }

    fn toolset(workers: JevWorkers) -> (DecisionToolset, DecisionRequestStore) {
        let (tools, store, _) = toolset_with_usage(workers);
        (tools, store)
    }

    fn toolset_with_usage(
        workers: JevWorkers,
    ) -> (DecisionToolset, DecisionRequestStore, crate::UsageState) {
        let store = DecisionRequestStore::default();
        let usage = crate::UsageState::new();
        let tools = DecisionToolset::new(workers, store.clone(), None, usage.clone()).unwrap();
        (tools, store, usage)
    }

    #[test]
    fn no_workers_means_no_tools() {
        assert!(
            DecisionToolset::new(
                Arc::new(BTreeMap::new()),
                Default::default(),
                None,
                crate::UsageState::new()
            )
            .is_none()
        );
    }

    #[test]
    fn questions_accept_options_levels_and_strings() {
        let parsed = parse_questions(Some(json!([
            {"id": "owner", "instruction": "Who?", "options": {"a": "A", "b": "B"}},
            {"instruction": "How bad?", "levels": ["low", "high"]},
            {"instruction": "Real?", "yes": "y", "no": "n"},
        ])))
        .unwrap();
        assert!(matches!(parsed[0], JevQuestion::Choice { .. }));
        assert!(matches!(parsed[1], JevQuestion::Score { .. }));
        assert!(matches!(parsed[2], JevQuestion::Noul { .. }));

        let fenced = "```json\n[{\"instruction\": \"Real?\"}]\n```";
        assert_eq!(parse_questions(Some(json!(fenced))).unwrap().len(), 1);
    }

    #[test]
    fn unusable_questions_return_the_help_text() {
        for bad in [
            json!("no array here"),
            json!(3),
            json!({"instruction": "x"}),
        ] {
            let err = parse_questions(Some(bad)).unwrap_err();
            assert!(err.contains("must be a JSON array"), "{err}");
        }
        let err = parse_questions(Some(json!([
            {"instruction": "x", "options": {"a": "A"}, "levels": ["l", "h"]}
        ])))
        .unwrap_err();
        assert!(err.contains("both `options` and `levels`"), "{err}");
        let err = parse_questions(Some(json!([{"instruction": "x", "critera": []}]))).unwrap_err();
        assert!(err.contains("critera"), "{err}");
    }

    #[test]
    fn state_strings_holding_json_are_parsed() {
        assert_eq!(
            parse_state(Some(json!("{\"a\": 1}"))),
            Some(json!({"a": 1}))
        );
        assert_eq!(
            parse_state(Some(json!("plain text"))),
            Some(json!("plain text"))
        );
        assert_eq!(parse_state(Some(json!({"a": 1}))), Some(json!({"a": 1})));
        assert_eq!(parse_state(Some(Value::Null)), None);
    }

    #[test]
    fn worker_is_required_only_when_ambiguous() {
        let one = workers(&["triage"], 0);
        assert_eq!(resolve_worker(&one, None).unwrap().name, "triage");
        let two = workers(&["a", "b"], 0);
        assert!(
            resolve_worker(&two, None)
                .unwrap_err()
                .contains("name the decision worker")
        );
        assert!(
            resolve_worker(&two, Some("zzz"))
                .unwrap_err()
                .contains("not a decision worker")
        );
    }

    #[tokio::test]
    async fn evaluate_returns_answers_and_summary() {
        let fake = FakeTransport::new([Ok(json!({
            "model": "jev-test",
            "answers": {"real": {"type": "noul", "noul": 0.8}},
            "usage": {"input_tokens": 5, "output_tokens": 1}
        }))]);
        let (tools, _) = toolset(with_fake(&workers(&["triage"], 0), fake.clone()));
        let out = tools
            .evaluate
            .call(EvaluateDecisionArgs {
                worker: None,
                questions: Some(json!([{"id": "real", "instruction": "Is it real?"}])),
                state: Some(json!("{\"alert\": \"disk full\"}")),
            })
            .await
            .unwrap();
        assert!(out.error.is_none(), "{out}");
        assert!(
            out.to_string().contains("real: Is it real? → P(yes)=0.80"),
            "{out}"
        );
        let sent: Value = serde_json::from_str(&fake.requests()[0]).unwrap();
        assert_eq!(sent["state"]["alert"], "disk full");
    }

    #[tokio::test]
    async fn evaluate_rejects_bad_input_without_calling_jev() {
        let fake = FakeTransport::new([]);
        let (tools, _) = toolset(with_fake(&workers(&["triage"], 0), fake.clone()));
        let missing_state = tools
            .evaluate
            .call(EvaluateDecisionArgs {
                worker: None,
                questions: Some(json!([{"instruction": "Real?"}])),
                state: None,
            })
            .await
            .unwrap();
        assert!(missing_state.to_string().contains("`state` is required"));
        let bad_score = tools
            .evaluate
            .call(EvaluateDecisionArgs {
                worker: None,
                questions: Some(json!([{"instruction": "Rate", "levels": ["only"]}])),
                state: Some(json!("x")),
            })
            .await
            .unwrap();
        assert!(bad_score.to_string().contains("2-10 levels"), "{bad_score}");
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn prepare_stores_and_replaces_requests() {
        let (tools, store) = toolset(workers(&["triage"], 1));
        let out = tools
            .prepare
            .call(PrepareDecisionArgs {
                name: "sev".to_string(),
                worker: None,
                questions: Some(json!([{"instruction": "Extra?"}])),
                state: Some(json!("{\"k\": 1}")),
            })
            .await
            .unwrap();
        assert!(out.status.contains("prepared"), "{out}");
        assert_eq!(out.question_ids, ["c0", "q1"]);
        let stored = store.lock().unwrap()["sev"].clone();
        assert_eq!(stored.worker, "triage");
        assert_eq!(stored.state, Some(json!({"k": 1})));

        let again = tools
            .prepare
            .call(PrepareDecisionArgs {
                name: "sev".to_string(),
                worker: None,
                questions: None,
                state: None,
            })
            .await
            .unwrap();
        assert!(again.status.contains("replaced"), "{again}");
        assert!(store.lock().unwrap()["sev"].questions.is_empty());
    }

    #[tokio::test]
    async fn prepare_rejects_ids_clashing_with_configured_questions() {
        let (tools, store) = toolset(workers(&["triage"], 1));
        let out = tools
            .prepare
            .call(PrepareDecisionArgs {
                name: "sev".to_string(),
                worker: None,
                questions: Some(json!([{"id": "c0", "instruction": "Clash?"}])),
                state: None,
            })
            .await
            .unwrap();
        assert!(out.status.contains("duplicate question id `c0`"), "{out}");
        assert!(store.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn prepare_needs_a_question_somewhere() {
        let (tools, _) = toolset(workers(&["triage"], 0));
        let out = tools
            .prepare
            .call(PrepareDecisionArgs {
                name: "empty".to_string(),
                worker: None,
                questions: None,
                state: None,
            })
            .await
            .unwrap();
        assert!(out.status.contains("no questions to ask"), "{out}");
    }

    #[tokio::test]
    async fn evaluate_adds_its_tokens_to_the_run_usage() {
        let fake = FakeTransport::new([
            Ok(json!({
                "model": "jev-test",
                "answers": {"real": {"type": "noul", "noul": 0.8}},
                "usage": {"input_tokens": 40, "output_tokens": 3}
            })),
            Err(401),
        ]);
        let (tools, _, usage) = toolset_with_usage(with_fake(&workers(&["triage"], 0), fake));
        let args = || EvaluateDecisionArgs {
            worker: None,
            questions: Some(json!([{"id": "real", "instruction": "Is it real?"}])),
            state: Some(json!("disk full")),
        };
        let ok = tools.evaluate.call(args()).await.unwrap();
        assert!(ok.error.is_none(), "{ok}");
        let failed = tools.evaluate.call(args()).await.unwrap();
        assert!(failed.error.is_some(), "{failed}");
        assert_eq!(usage.get_final_usage(), (40, 3, 43));
    }

    #[tokio::test]
    async fn evaluate_rejects_state_over_the_budget_without_calling_jev() {
        let fake = FakeTransport::new([]);
        let (tools, _, usage) =
            toolset_with_usage(with_fake(&workers(&["triage"], 0), fake.clone()));
        let out = tools
            .evaluate
            .call(EvaluateDecisionArgs {
                worker: None,
                questions: Some(json!([{"instruction": "Real?"}])),
                state: Some(
                    json!({"blob": "x".repeat(crate::orchestration::jev::JEV_MAX_STATE_CHARS)}),
                ),
            })
            .await
            .unwrap();
        assert!(out.to_string().contains("keep it under"), "{out}");
        assert!(fake.requests().is_empty());
        assert_eq!(usage.get_final_usage(), (0, 0, 0));
    }

    #[tokio::test]
    async fn prepare_rejects_oversized_state_without_storing_it() {
        let (tools, store) = toolset(workers(&["triage"], 1));
        let out = tools
            .prepare
            .call(PrepareDecisionArgs {
                name: "big".to_string(),
                worker: None,
                questions: None,
                state: Some(json!({
                    "blob": "x".repeat(crate::orchestration::jev::JEV_MAX_STAGED_STATE_CHARS)
                })),
            })
            .await
            .unwrap();
        assert!(out.status.contains("keep it under"), "{out}");
        assert!(store.lock().unwrap().is_empty());
    }
}
