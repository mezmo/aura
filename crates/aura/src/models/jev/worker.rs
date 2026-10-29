//! A configured Jev worker: its config and the jev-driver client built from it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use aura_config::{JevQuestion, JevWorkerConfig, OrchestrationConfig};
use jev_driver::JevError;
use jev_driver::client::{DEFAULT_MODEL, JevClient, JevConfig};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::answer::Answers;
use super::decision::Decision;

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
mod tests {
    use serde_json::json;

    use super::*;

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
